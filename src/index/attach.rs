//! Attachments in the index: the persisted scope (types, max size), the
//! sync phase (list, skip before reading, hash, extract each unseen content
//! once), removal of unlinked contents, counts.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use super::db_err;
use super::doctor::ExtractFn;
use super::extract::{self, Kind, Limits, Skip};
use super::sync::{MailSource, get_meta, set_meta};
use super::text::chunk;
use crate::error::{Error, Result};
use crate::ndb::Nid;
use crate::store::Attachment;
use crate::timeutil::now_ft;

/// Extraction attempts of a failed or timed-out content, one per sync.
pub const MAX_ATTEMPTS: i64 = 3;
/// Largest max size the scope accepts.
pub const MAX_BYTES_LIMIT: u64 = 100 << 20;
/// Skip reason of a supported type left out of the scope.
const EXCLUDED_TYPE: &str = "excluded_type";

/// Persisted attachment scope (meta `attachment_scope`). Default: every
/// supported type, 20 MB.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scope {
    pub types: Vec<Kind>,
    pub max_bytes: u64,
}

impl Default for Scope {
    fn default() -> Scope {
        Scope { types: Kind::ALL.to_vec(), max_bytes: Limits::default().max_bytes as u64 }
    }
}

pub fn scope(c: &Connection) -> Result<Scope> {
    match get_meta(c, "attachment_scope")? {
        Some(s) => serde_json::from_str(&s).map_err(|e| Error::Format(format!("stored attachment scope: {e}"))),
        None => Ok(Scope::default()),
    }
}

/// Store the scope. Stored contents outside it are kept, through later
/// syncs too (`counts`: `out_of_scope`). Messages with an attachment skipped as too large or of
/// an excluded type are read again by the next sync; returns their number.
pub fn set_scope(c: &Connection, s: &Scope) -> Result<usize> {
    if s.types.is_empty() {
        return Err(Error::Usage("at least one attachment type is needed".into()));
    }
    if s.max_bytes == 0 || s.max_bytes > MAX_BYTES_LIMIT {
        return Err(Error::Usage(format!("attachment max size must be 1 byte to {} MB", MAX_BYTES_LIMIT >> 20)));
    }
    set_meta(c, "attachment_scope", &serde_json::to_string(s).map_err(|e| Error::Io(e.to_string()))?)?;
    c.execute(
        "UPDATE messages SET attachments_done = NULL WHERE id IN
            (SELECT message FROM message_attachments WHERE skip IN ('too_large', 'excluded_type'))",
        [],
    )
    .map_err(db_err)
}

/// Messages whose attachments are still to read.
pub fn pending(c: &Connection) -> Result<i64> {
    c.query_row("SELECT COUNT(*) FROM messages WHERE has_attachments = 1 AND attachments_done IS NULL", [], |r| {
        r.get(0)
    })
    .map_err(db_err)
}

/// Delete the contents no attachment links to, with their chunks and FTS rows.
pub fn gc(c: &Connection) -> Result<()> {
    c.execute_batch(
        "DELETE FROM attachment_chunks WHERE sha256 NOT IN
            (SELECT sha256 FROM message_attachments WHERE sha256 IS NOT NULL);
         DELETE FROM attachment_contents WHERE sha256 NOT IN
            (SELECT sha256 FROM message_attachments WHERE sha256 IS NOT NULL);",
    )
    .map_err(db_err)
}

/// Counts of one attachment phase.
#[derive(Default)]
pub struct Stats {
    pub extracted: u64,
    pub deduplicated: u64,
    /// Extractions that failed or timed out.
    pub failed: u64,
    pub skipped: BTreeMap<&'static str, u64>,
    /// Read errors by kind; the message is read again by the next sync.
    pub errors: BTreeMap<&'static str, u64>,
    pub interrupted: bool,
}

/// Read the attachments of the pending messages, newest first, `batch`
/// messages per transaction. Before each batch but the first (or every one
/// when `ran` is set), stop if `over()`. A message is done when each of its
/// attachments is: skipped, stored, deduplicated, or failed `MAX_ATTEMPTS`
/// times; else the next sync reads it again.
pub fn phase(
    c: &mut Connection,
    src: &mut dyn MailSource,
    x: &mut ExtractFn,
    batch: usize,
    ran: bool,
    over: &dyn Fn() -> bool,
    p: &mut super::progress::Progress,
) -> Result<Stats> {
    let s = scope(c)?;
    let todo: Vec<(i64, Nid)> = c
        .prepare(
            "SELECT m.id, MIN(l.nid) FROM messages m JOIN locations l ON l.message = m.id
             WHERE m.has_attachments = 1 AND m.attachments_done IS NULL
             GROUP BY m.id ORDER BY m.received DESC, m.id DESC",
        )
        .map_err(db_err)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    let (mut st, mut tried) = (Stats::default(), HashSet::new());
    p.phase(Some(c), "attachments", todo.len() as u64)?;
    let mut done = 0;
    for (i, group) in todo.chunks(batch.max(1)).enumerate() {
        if (i > 0 || ran) && over() {
            st.interrupted = true;
            break;
        }
        let tx = c.transaction().map_err(db_err)?;
        for &(id, nid) in group {
            let list = match src.attachments(nid) {
                Ok(v) => v,
                Err(e) => {
                    *st.errors.entry(e.kind()).or_default() += 1;
                    continue;
                }
            };
            // Links to attachments no longer listed: removed from the source
            // (`gc` below collects their content).
            let idx: Vec<usize> = list.iter().map(|a| a.index).collect();
            tx.execute(
                "DELETE FROM message_attachments WHERE message = ?1 AND idx NOT IN (SELECT value FROM json_each(?2))",
                params![id, serde_json::to_string(&idx).unwrap()],
            )
            .map_err(db_err)?;
            let mut done = true;
            for a in &list {
                done &= one(&tx, src, x, &s, (id, nid), a, &mut st, &mut tried)?;
            }
            if done {
                tx.execute("UPDATE messages SET attachments_done = 1 WHERE id = ?1", [id]).map_err(db_err)?;
            }
        }
        // A link may have changed content.
        gc(&tx)?;
        done += group.len() as u64;
        p.update(Some(&tx), done)?;
        tx.commit().map_err(db_err)?;
    }
    Ok(st)
}

/// Skip reason from the attachment's properties alone (no data read).
fn before_read(a: &Attachment, s: &Scope) -> Option<&'static str> {
    let l = Limits { max_bytes: s.max_bytes as usize, ..Limits::default() };
    if let Some(r) = extract::skip_attachment(a, &l) {
        return Some(r.as_str());
    }
    match extract::declared(a.name.as_deref(), a.mime_type.as_deref()) {
        Some(Ok(k)) if !s.types.contains(&k) => Some(EXCLUDED_TYPE),
        // An unknown extension: not sniffed.
        None if a.name.as_deref().and_then(|n| Path::new(n).extension()).is_some() => {
            Some(Skip::UnsupportedType.as_str())
        }
        _ => None,
    }
}

/// Store the link of attachment `a` of `message`.
fn link(tx: &Connection, message: i64, a: &Attachment, sha: Option<&str>, skip: Option<&str>) -> Result<()> {
    tx.execute(
        "INSERT INTO message_attachments(message, idx, name, size, mime, sha256, skip)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(message, idx) DO UPDATE SET name = excluded.name, size = excluded.size, mime = excluded.mime,
            sha256 = excluded.sha256, skip = excluded.skip",
        params![message, a.index as i64, a.name, a.size, a.mime_type, sha, skip],
    )
    .map(drop)
    .map_err(db_err)
}

/// Skip `a`. Out of the scope (type, size), a stored content stays linked
/// while the attachment is the same (properties, and data `sha` when read):
/// only a changed or removed attachment unlinks it, and `gc` collects a
/// content with its last link.
fn skipped(
    tx: &Connection,
    st: &mut Stats,
    (message, a): (i64, &Attachment),
    reason: &'static str,
    sha: Option<&str>,
) -> Result<bool> {
    *st.skipped.entry(reason).or_default() += 1;
    let scope = reason == EXCLUDED_TYPE || reason == Skip::TooLarge.as_str();
    let same: bool = scope
        && tx
            .query_row(
                "SELECT 1 FROM message_attachments WHERE message = ?1 AND idx = ?2 AND sha256 IS NOT NULL
                 AND name IS ?3 AND size IS ?4 AND mime IS ?5 AND (?6 IS NULL OR sha256 = ?6)",
                params![message, a.index as i64, a.name, a.size, a.mime_type, sha],
                |_| Ok(()),
            )
            .optional()
            .map_err(db_err)?
            .is_some();
    if !same {
        link(tx, message, a, None, Some(reason))?;
    }
    Ok(true)
}

fn retry(outcome: &str, attempts: i64) -> bool {
    matches!(outcome, "failed" | "timeout") && attempts < MAX_ATTEMPTS
}

/// One attachment: skip, deduplicate or extract, then link. True when no
/// retry is needed.
#[allow(clippy::too_many_arguments)]
fn one(
    tx: &Connection,
    src: &mut dyn MailSource,
    x: &mut ExtractFn,
    s: &Scope,
    (message, nid): (i64, Nid),
    a: &Attachment,
    st: &mut Stats,
    tried: &mut HashSet<String>,
) -> Result<bool> {
    if let Some(r) = before_read(a, s) {
        return skipped(tx, st, (message, a), r, None);
    }
    let data = match src.attachment_data(nid, a) {
        Ok(d) => d,
        Err(e) => {
            *st.errors.entry(e.kind()).or_default() += 1;
            return Ok(false);
        }
    };
    let (name, mime) = (a.name.as_deref(), a.mime_type.as_deref());
    let sha = format!("{:x}", Sha256::digest(&data));
    if data.len() as u64 > s.max_bytes {
        return skipped(tx, st, (message, a), Skip::TooLarge.as_str(), Some(&sha));
    }
    let kind = match extract::kind(&data, name, mime) {
        Ok(k) if s.types.contains(&k) => k,
        Ok(_) => return skipped(tx, st, (message, a), EXCLUDED_TYPE, Some(&sha)),
        Err(r) => return skipped(tx, st, (message, a), r.as_str(), Some(&sha)),
    };
    let prev: Option<(String, i64)> = tx
        .query_row("SELECT outcome, attempts FROM attachment_contents WHERE sha256 = ?1", [&sha], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()
        .map_err(db_err)?;
    // Seen before (or already tried by this sync): link only.
    if let Some((o, n)) = prev
        && (!retry(&o, n) || tried.contains(&sha))
    {
        st.deduplicated += 1;
        link(tx, message, a, Some(&sha), None)?;
        return Ok(!retry(&o, n));
    }
    tried.insert(sha.clone());
    let r = x(&data, name, mime);
    let (outcome, chars, truncated) = match &r {
        Ok(e) => ("ok", Some(e.text.chars().count() as i64), e.truncated),
        Err(r) => (r.as_str(), None, false),
    };
    let attempts: i64 = tx
        .query_row(
            "INSERT INTO attachment_contents(sha256, kind, size, outcome, chars, truncated, attempts, extracted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)
             ON CONFLICT(sha256) DO UPDATE SET kind = excluded.kind, outcome = excluded.outcome,
                chars = excluded.chars, truncated = excluded.truncated, attempts = attempts + 1,
                extracted_at = excluded.extracted_at
             RETURNING attempts",
            params![sha, kind.as_str(), data.len() as i64, outcome, chars, truncated, now_ft()],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    match r {
        Ok(e) => {
            // Every chunk is indexed for keyword search (no cap here).
            for (seq, (t, rg)) in chunk(&e.text).iter().enumerate() {
                tx.execute(
                    "INSERT INTO attachment_chunks(sha256, seq, text, start_char, end_char) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![sha, seq as i64, t, rg.start as i64, rg.end as i64],
                )
                .map_err(db_err)?;
            }
            st.extracted += 1;
        }
        Err(Skip::Failed | Skip::Timeout) => st.failed += 1,
        Err(r) => *st.skipped.entry(r.as_str()).or_default() += 1,
    }
    link(tx, message, a, Some(&sha), None)?;
    Ok(!retry(outcome, attempts))
}

/// Counts for `status` and `scope`: fixed labels and numbers only.
pub fn counts(c: &Connection) -> Result<Json> {
    let n = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).map_err(db_err);
    let by = |sql: &str| -> Result<BTreeMap<String, i64>> {
        c.prepare(sql)
            .map_err(db_err)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_err)
    };
    let s = scope(c)?;
    let out: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM attachment_contents
             WHERE kind NOT IN (SELECT value FROM json_each(?1)) OR size > ?2",
            params![serde_json::to_string(&s.types).unwrap(), s.max_bytes as i64],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    Ok(json!({
        "attachments": n("SELECT COUNT(*) FROM message_attachments")?,
        "contents": n("SELECT COUNT(sha256) FROM message_attachments")?,
        "distinct": n("SELECT COUNT(*) FROM attachment_contents")?,
        "chunks": n("SELECT COUNT(*) FROM attachment_chunks")?,
        "by_outcome": by("SELECT outcome, COUNT(*) FROM attachment_contents GROUP BY outcome")?,
        "skipped_by_reason": by("SELECT skip, COUNT(*) FROM message_attachments WHERE skip IS NOT NULL GROUP BY skip")?,
        "pending_messages": pending(c)?,
        "scope": s,
        // Stored contents kept after the scope was narrowed.
        "out_of_scope": out,
    }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::index::extract::Outcome;
    use crate::index::extract::tests::{docx, pdf};
    use crate::index::migrate;
    use crate::index::sync::fake::*;
    use crate::index::sync::{Options, sync, sync_with};

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// In-process extraction: "FAIL" in the data fails, "HANG" times out.
    fn fake_extract(b: &[u8], n: Option<&str>, m: Option<&str>) -> Outcome {
        let has = |s: &[u8]| b.windows(s.len()).any(|w| w == s);
        if has(b"FAIL") {
            Err(Skip::Failed)
        } else if has(b"HANG") {
            Err(Skip::Timeout)
        } else {
            extract::extract(b, n, m)
        }
    }

    /// Sync with attachments; returns the summary and the extraction calls.
    fn run(c: &mut Connection, f: &mut Fake, o: &Options) -> (Json, usize) {
        let mut calls = 0;
        let mut x = |b: &[u8], n: Option<&str>, m: Option<&str>| {
            calls += 1;
            fake_extract(b, n, m)
        };
        let s = sync_with(c, f, Some(&mut x), o).unwrap();
        (s, calls)
    }

    fn fts(c: &Connection, word: &str) -> i64 {
        count(c, &format!("SELECT COUNT(*) FROM attachment_fts WHERE attachment_fts MATCH '{word}'"))
    }

    /// mailbox() plus 0x604 with a PDF, a DOCX and an inline image.
    fn with_files() -> Fake {
        let mut f = mailbox();
        f.add(0x604, 0x22, Some("<p1@fict.invalid>"), "Files", "See the files.");
        f.attach(0x604, "SECRET_offer.pdf", None, false, &pdf(&["Zorbulex quarterly offer"]));
        f.attach(0x604, "SECRET_minutes.docx", None, false, &docx(&["Minutes of the Plimsoll meeting"]));
        f.attach(0x604, "SECRET_logo.png", Some("image/png"), true, b"\x89PNG fictitious");
        f
    }

    #[test]
    fn new_message_pdf_docx_inline_image() {
        let (mut c, mut f) = (db(), with_files());
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["attachments_extracted"].as_u64(), calls), (Some(2), 2), "{s}");
        assert_eq!(s["attachments_skipped_by_reason"], json!({ "inline": 1 }), "{s}");
        assert_eq!((s["attachments_deduplicated"].as_u64(), s["attachments_failed"].as_u64()), (Some(0), Some(0)));
        assert_eq!((s["complete"].as_bool(), s["attachments_remaining"].as_u64()), (Some(true), Some(0)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_contents"), 2);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM message_attachments"), 3);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM message_attachments WHERE sha256 IS NULL AND skip = 'inline'"), 1);
        assert_eq!(f.reads, [(0x604, 0), (0x604, 1)], "the inline image is never read");
        assert_eq!((fts(&c, "zorbulex"), fts(&c, "plimsoll")), (1, 1));
        let kinds: String = c
            .query_row("SELECT group_concat(kind || ':' || outcome, ',') FROM (SELECT * FROM attachment_contents ORDER BY kind)", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kinds, "docx:ok,pdf:ok");
        assert_eq!(f.listed, [0x604], "only the message with attachments is listed");
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["unchanged"].as_bool(), calls), (Some(true), 0), "{s}");
    }

    #[test]
    fn same_file_in_two_messages_once() {
        let (mut c, mut f) = (db(), mailbox());
        let offer = pdf(&["Zorbulex quarterly offer"]);
        f.add(0x604, 0x22, Some("<p1@fict.invalid>"), "Offer", "Offer attached.");
        f.attach(0x604, "offer.pdf", None, false, &offer);
        f.add(0x624, 0x42, Some("<p2@fict.invalid>"), "Fwd: offer", "Forwarding.");
        f.attach(0x624, "offer copy.pdf", None, false, &offer);
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["attachments_extracted"].as_u64(), s["attachments_deduplicated"].as_u64()), (Some(1), Some(1)));
        assert_eq!(calls, 1, "extracted once");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_contents"), 1);
        assert_eq!(count(&c, "SELECT COUNT(DISTINCT sha256) FROM message_attachments"), 1);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM message_attachments"), 2);
        let chunks = count(&c, "SELECT COUNT(*) FROM attachment_chunks");
        assert_eq!((chunks, fts(&c, "zorbulex")), (1, 1), "chunks stored once");
        // One copy deleted: the content stays.
        f.remove(0x604);
        run(&mut c, &mut f, &Options::default());
        assert_eq!(count(&c, "SELECT COUNT(*) FROM message_attachments"), 1);
        assert_eq!((count(&c, "SELECT COUNT(*) FROM attachment_contents"), fts(&c, "zorbulex")), (1, 1));
        // Both deleted: content, chunks and FTS rows go.
        f.remove(0x624);
        let (s, _) = run(&mut c, &mut f, &Options::default());
        assert_eq!(s["deleted"], 1, "{s}");
        for t in ["message_attachments", "attachment_contents", "attachment_chunks"] {
            assert_eq!(count(&c, &format!("SELECT COUNT(*) FROM {t}")), 0, "{t}");
        }
        assert_eq!(fts(&c, "zorbulex"), 0);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_fts"), 0);
    }

    #[test]
    fn skipped_without_reading() {
        let (mut c, mut f) = (db(), mailbox());
        f.add(0x604, 0x22, Some("<s1@fict.invalid>"), "Many", "Many files.");
        f.attach(0x604, "big.pdf", None, false, b"%PDF- small data").size = Some(30 << 20);
        f.attach(0x604, "setup.exe", None, false, b"MZ fictitious");
        f.attach(0x604, "photo.JPG", None, false, b"\xFF\xD8\xFF fictitious");
        f.attach(0x604, "logo.gif", None, true, b"GIF8 fictitious");
        f.attach(0x604, "fwd", None, false, b"").method = "embedded_message";
        f.attach(0x604, "link.pdf", None, false, b"").method = "by_reference";
        // Unknown from the name: read, then typed by its bytes.
        f.attach(0x604, "", None, false, &pdf(&["Nameless Quillfeather report"]));
        f.attach(0x604, "README", None, false, b"MZ fictitious binary");
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!(f.reads, [(0x604, 6), (0x604, 7)], "{s}");
        assert_eq!(calls, 1, "{s}");
        assert_eq!(
            s["attachments_skipped_by_reason"],
            json!({ "embedded_message": 1, "image": 1, "inline": 1, "too_large": 1, "unsupported_type": 3 }),
            "{s}"
        );
        assert_eq!((s["attachments_extracted"].as_u64(), s["complete"].as_bool()), (Some(1), Some(true)), "{s}");
        assert_eq!(fts(&c, "quillfeather"), 1);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM message_attachments WHERE sha256 IS NULL"), 7);
    }

    #[test]
    fn failures_counted_retried_then_final() {
        let (mut c, mut f) = (db(), mailbox());
        f.add(0x604, 0x22, Some("<x1@fict.invalid>"), "Broken", "Broken files.");
        f.attach(0x604, "a.pdf", None, false, b"%PDF- FAIL fictitious");
        f.attach(0x604, "b.txt", None, false, b"HANG fictitious");
        f.attach(0x604, "c.txt", None, false, b"Fine Wobblecrank notes");
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!(
            (calls, s["attachments_failed"].as_u64(), s["attachments_extracted"].as_u64()),
            (3, Some(2), Some(1))
        );
        assert_eq!((s["complete"].as_bool(), s["attachments_remaining"].as_u64()), (Some(false), Some(1)), "{s}");
        assert_eq!(fts(&c, "wobblecrank"), 1, "the good file is stored");
        // Same revision: retried, the good file deduplicated.
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["unchanged"].as_bool(), calls), (Some(false), 2), "{s}");
        assert_eq!((s["attachments_failed"].as_u64(), s["attachments_deduplicated"].as_u64()), (Some(2), Some(1)));
        // Third attempt: final.
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((calls, s["complete"].as_bool(), s["attachments_remaining"].as_u64()), (2, Some(true), Some(0)));
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["unchanged"].as_bool(), calls), (Some(true), 0), "{s}");
        let v = counts(&c).unwrap();
        assert_eq!(v["by_outcome"], json!({ "failed": 1, "ok": 1, "timeout": 1 }), "{v}");
        assert_eq!(count(&c, "SELECT MAX(attempts) FROM attachment_contents"), MAX_ATTEMPTS);
        // A read error leaves the message pending, nothing stored for that file.
        f.add(0x624, 0x22, Some("<x2@fict.invalid>"), "Gone", "x");
        f.attach(0x624, "gone.txt", None, false, b"x");
        f.files.get_mut(&0x624).unwrap()[0].0.index = 9;
        let (s, _) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["attachments_errors"]["not_found"].as_u64(), s["complete"].as_bool()), (Some(1), Some(false)));
    }

    #[test]
    fn backfill_resumable() {
        let (mut c, mut f) = (db(), mailbox());
        for i in 0..7u32 {
            let nid = 0x1004 + i * 0x20;
            f.add(nid, 0x22, Some(&format!("<b{i}@fict.invalid>")), "Bulk", "Bulk text.");
            f.attach(nid, &format!("n{i}.txt"), None, false, format!("Backfill note {i}").as_bytes());
        }
        // Indexed without attachments (as before this slice, or --no-attachments).
        let s = sync(&mut c, &mut f, &Options::default()).unwrap();
        assert_eq!((s["attachments_remaining"].as_u64(), s["complete"].as_bool()), (Some(7), Some(true)), "{s}");
        assert!(f.listed.is_empty() && f.reads.is_empty());
        assert_eq!(sync(&mut c, &mut f, &Options::default()).unwrap()["unchanged"], true);
        let o = Options { batch: 3, max_time: Some(Duration::ZERO), ..Options::default() };
        let (s, _) = run(&mut c, &mut f, &o);
        assert_eq!((s["unchanged"].as_bool(), s["attachments_extracted"].as_u64()), (Some(false), Some(3)), "{s}");
        assert_eq!((s["complete"].as_bool(), s["attachments_remaining"].as_u64()), (Some(false), Some(4)), "{s}");
        assert_eq!(f.listed, [0x10c4, 0x10a4, 0x1084], "newest first");
        assert_eq!(run(&mut c, &mut f, &o).0["attachments_remaining"], 1);
        let (s, _) = run(&mut c, &mut f, &o);
        assert_eq!((s["attachments_extracted"].as_u64(), s["complete"].as_bool()), (Some(1), Some(true)), "{s}");
        assert_eq!(f.listed.len(), 7, "nothing listed twice");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_contents"), 7);
        assert_eq!(run(&mut c, &mut f, &o).0["unchanged"], true);
    }

    #[test]
    fn scope_types_and_size() {
        let (mut c, mut f) = (db(), with_files());
        assert_eq!(scope(&c).unwrap(), Scope::default());
        assert_eq!(Scope::default().max_bytes, 20 << 20);
        set_scope(&c, &Scope { types: vec![Kind::Pdf], ..Scope::default() }).unwrap();
        let (s, _) = run(&mut c, &mut f, &Options::default());
        assert_eq!(s["attachments_skipped_by_reason"], json!({ "excluded_type": 1, "inline": 1 }), "{s}");
        assert_eq!(f.reads, [(0x604, 0)], "excluded type not read");
        // Narrower: the stored PDF stays, reported out of scope.
        set_scope(&c, &Scope { types: vec![Kind::Docx], ..Scope::default() }).unwrap();
        let v = counts(&c).unwrap();
        assert_eq!((v["distinct"].as_u64(), v["out_of_scope"].as_u64()), (Some(1), Some(1)), "{v}");
        assert_eq!(fts(&c, "zorbulex"), 1);
        // Wider: the skipped DOCX is picked up by the next sync.
        let r = set_scope(&c, &Scope::default()).unwrap();
        assert_eq!(r, 1, "one message queued again");
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["attachments_extracted"].as_u64(), calls), (Some(1), 1), "{s}");
        assert_eq!(fts(&c, "plimsoll"), 1);
        // Size limit, checked before reading.
        set_scope(&c, &Scope { max_bytes: 10, ..Scope::default() }).unwrap();
        f.add(0x644, 0x22, Some("<p3@fict.invalid>"), "More", "x");
        f.attach(0x644, "more.txt", None, false, b"more than ten bytes");
        let reads = f.reads.len();
        let (s, _) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["attachments_skipped_by_reason"]["too_large"].as_u64(), f.reads.len()), (Some(1), reads));
        for bad in [Scope { types: vec![], ..Scope::default() }, Scope { max_bytes: 0, ..Scope::default() }] {
            assert_eq!(set_scope(&c, &bad).unwrap_err().kind(), "usage");
        }
    }

    /// B2: a stored, embedded DOCX next to an oversized PDF. Narrowing the
    /// types to PDF and syncing keeps the DOCX text, FTS rows and vectors
    /// (counted out of scope); widening back extracts and embeds nothing;
    /// deleting the last message still collects everything.
    #[test]
    fn narrowed_scope_keeps_stored_content() {
        let (mut c, mut f) = (db(), mailbox());
        f.add(0x604, 0x22, Some("<k1@fict.invalid>"), "Files", "See the files.");
        f.attach(0x604, "minutes.docx", None, false, &docx(&["Minutes of the Plimsoll meeting"]));
        f.attach(0x604, "big.pdf", None, false, b"%PDF- small data").size = Some(30 << 20);
        let (_, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!(calls, 1);
        let m = crate::index::model::get("e5-small").unwrap();
        c.execute(
            "INSERT INTO attachment_vectors SELECT id, ?1, ?2, zeroblob(?3) FROM attachment_chunks",
            params![m.name, m.revision, (m.dims * 4) as i64],
        )
        .unwrap();
        for (k, v) in [("embed_model", m.name), ("embed_revision", m.revision)] {
            set_meta(&c, k, v).unwrap();
        }
        let stored = |c: &Connection| {
            [
                count(c, "SELECT COUNT(*) FROM attachment_contents"),
                count(c, "SELECT COUNT(*) FROM attachment_chunks"),
                count(c, "SELECT COUNT(*) FROM attachment_vectors"),
                fts(c, "plimsoll"),
                count(c, "SELECT COUNT(*) FROM message_attachments WHERE sha256 IS NOT NULL"),
            ]
        };
        let before = stored(&c);
        assert_eq!(before, [1, 1, 1, 1, 1]);

        assert_eq!(set_scope(&c, &Scope { types: vec![Kind::Pdf], ..Scope::default() }).unwrap(), 1);
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["unchanged"].as_bool(), calls), (Some(false), 0), "{s}");
        assert_eq!(stored(&c), before, "kept after a narrowed sync");
        let v = counts(&c).unwrap();
        assert_eq!((v["distinct"].as_u64(), v["out_of_scope"].as_u64()), (Some(1), Some(1)), "{v}");
        let st = crate::index::search::status(&c).unwrap();
        assert_eq!(st["attachments"]["out_of_scope"], 1, "{st}");
        assert_eq!(st["embedding"]["attachment_chunks"]["out_of_scope_embedded"], 1, "{st}");

        set_scope(&c, &Scope::default()).unwrap();
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((calls, s["attachments_extracted"].as_u64()), (0, Some(0)), "no re-extraction: {s}");
        assert_eq!(stored(&c), before);
        let v = crate::index::vectors::counts(&c).unwrap();
        assert_eq!(v["attachment_chunks"]["pending"], 0, "no re-embedding: {v}");

        // Below a narrower size limit: kept the same way.
        set_scope(&c, &Scope { max_bytes: 10, ..Scope::default() }).unwrap();
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((calls, s["attachments_skipped_by_reason"]["too_large"].as_u64()), (0, Some(2)), "{s}");
        assert_eq!((stored(&c), counts(&c).unwrap()["out_of_scope"].as_u64()), (before, Some(1)));
        set_scope(&c, &Scope::default()).unwrap();
        run(&mut c, &mut f, &Options::default());
        assert_eq!(stored(&c), before);

        // The source replaces the DOCX (same index, other file): the old content goes.
        f.files.get_mut(&0x604).unwrap()[0].0.size = Some(1);
        set_scope(&c, &Scope { types: vec![Kind::Pdf], ..Scope::default() }).unwrap();
        run(&mut c, &mut f, &Options::default());
        assert_eq!(stored(&c), [0, 0, 0, 0, 0], "replaced in the source");

        // Last reference deleted: normal GC.
        set_scope(&c, &Scope::default()).unwrap();
        run(&mut c, &mut f, &Options::default());
        assert_eq!(stored(&c), [1, 1, 0, 1, 1], "read again, to embed again");
        f.remove(0x604);
        run(&mut c, &mut f, &Options::default());
        assert_eq!(stored(&c), [0, 0, 0, 0, 0], "last reference gone");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_fts"), 0);
    }

    /// S1: attachments added, replaced and removed on an indexed message
    /// (same Message-ID) are picked up with the message's refresh; its body
    /// vector is kept while the body is unchanged; a stored content out of
    /// the scope stays (B2); a content shared with another message survives.
    #[test]
    fn changed_attachments_refreshed() {
        let (mut c, mut f) = (db(), mailbox());
        f.add(0x604, 0x22, Some("<c1@fict.invalid>"), "Files", "See the files.");
        f.add(0x624, 0x42, Some("<c2@fict.invalid>"), "Other", "Other files.");
        f.attach(0x624, "shared.txt", None, false, b"Shared Frobisher notes");
        run(&mut c, &mut f, &Options::default());
        let m = crate::index::model::get("e5-small").unwrap();
        c.execute(
            "INSERT INTO vectors SELECT id, ?1, ?2, zeroblob(?3) FROM chunks",
            params![m.name, m.revision, (m.dims * 4) as i64],
        )
        .unwrap();
        for (k, v) in [("embed_model", m.name), ("embed_revision", m.revision)] {
            set_meta(&c, k, v).unwrap();
        }
        let body_vec = |c: &Connection| {
            count(
                c,
                "SELECT COUNT(*) FROM vectors v JOIN chunks ch ON ch.id = v.chunk JOIN messages m ON m.id = ch.message
                 WHERE m.ident = 'mid:<c1@fict.invalid>'",
            )
        };
        let links = |c: &Connection| {
            count(
                c,
                "SELECT COUNT(*) FROM message_attachments a JOIN messages m ON m.id = a.message
                 WHERE m.ident = 'mid:<c1@fict.invalid>'",
            )
        };
        let flag =
            |c: &Connection| count(c, "SELECT has_attachments FROM messages WHERE ident = 'mid:<c1@fict.invalid>'");
        assert_eq!((body_vec(&c), links(&c), flag(&c)), (1, 0, 0));

        // Added: a new file and one another message already carries.
        f.attach(0x604, "plan.txt", None, false, b"Plan Gorblin draft");
        f.attach(0x604, "shared.txt", None, false, b"Shared Frobisher notes");
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!(
            (s["refreshed"].as_u64(), calls, s["attachments_deduplicated"].as_u64()),
            (Some(1), 1, Some(1)),
            "{s}"
        );
        assert_eq!((links(&c), flag(&c), fts(&c, "gorblin")), (2, 1, 1));
        assert_eq!(body_vec(&c), 1, "unchanged body: its vector is reused");

        // Replaced (same name, other file): the old content goes.
        f.replace(0x604, 0, b"Plan Hexwold final version");
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["refreshed"].as_u64(), calls), (Some(1), 1), "{s}");
        assert_eq!((fts(&c, "gorblin"), fts(&c, "hexwold"), links(&c)), (0, 1, 2));
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_contents"), 2);

        // Removed: the shared file stays for the other message.
        f.detach(0x604, 1);
        let (s, _) = run(&mut c, &mut f, &Options::default());
        assert_eq!(s["refreshed"], 1, "{s}");
        assert_eq!((links(&c), fts(&c, "frobisher"), fts(&c, "hexwold")), (1, 1, 1));
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_contents"), 2);

        // Body edited under a narrowed scope: the stored TXT is kept (B2).
        set_scope(&c, &Scope { types: vec![Kind::Pdf], ..Scope::default() }).unwrap();
        f.edit(0x604, "See the file, edited.");
        let (s, calls) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["refreshed"].as_u64(), calls), (Some(1), 0), "{s}");
        assert_eq!((links(&c), fts(&c, "hexwold"), body_vec(&c)), (1, 1, 0));
        assert_eq!(counts(&c).unwrap()["out_of_scope"], 2);
        set_scope(&c, &Scope::default()).unwrap();

        // Last file removed: the flag clears, the link and content go.
        f.detach(0x604, 0);
        let (s, _) = run(&mut c, &mut f, &Options::default());
        assert_eq!((s["refreshed"].as_u64(), s["complete"].as_bool()), (Some(1), Some(true)), "{s}");
        assert_eq!((links(&c), flag(&c), fts(&c, "hexwold"), fts(&c, "frobisher")), (0, 0, 0, 1));
        assert_eq!(count(&c, "SELECT COUNT(*) FROM attachment_contents"), 1);
    }

    #[test]
    fn no_attachments_option_skips_the_phase() {
        let (mut c, mut f) = (db(), with_files());
        let s = sync(&mut c, &mut f, &Options::default()).unwrap();
        assert_eq!(s["attachments"], "skipped", "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM message_attachments"), 0);
        assert!(f.listed.is_empty());
    }

    #[test]
    fn outputs_have_counts_only() {
        let (mut c, mut f) = (db(), with_files());
        f.add(0x624, 0x22, Some("<p9@fict.invalid>"), "SECRET_SUBJECT", "SECRET_BODY");
        f.attach(0x624, "SECRET_NAME_x.txt", Some("text/SECRET"), false, b"SECRET_CONTENT FAIL");
        f.attach(0x624, "SECRET_NAME.weird", None, false, b"SECRET_CONTENT");
        let (s, _) = run(&mut c, &mut f, &Options::default());
        set_scope(&c, &Scope { types: vec![Kind::Txt], ..Scope::default() }).unwrap();
        let outputs =
            [s.to_string(), counts(&c).unwrap().to_string(), crate::index::search::status(&c).unwrap().to_string()];
        for o in &outputs {
            for m in ["SECRET", "offer", "minutes", "logo", "Zorbulex", "zorbulex", "Plimsoll", "fict"] {
                assert!(!o.contains(m), "output leaks {m:?}: {o}");
            }
        }
        assert!(outputs[2].contains("\"attachments\""), "{}", outputs[2]);
    }
}
