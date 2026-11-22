//! Stored vectors (one row per chunk and model), the persisted embedding
//! scope and the resumable, newest-first embedding run.

use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::db_err;
use super::embed::Embedder;
use super::model::{self, Model};
use super::search::folder_ids;
use super::sync::{get_meta, set_meta};
use crate::error::{Error, Result, guarded};
use crate::timeutil::format_ft;

/// What the embedding run needs from a model: the in-process model, a
/// worker process, or a deterministic fake in tests.
pub trait Embed: Send {
    fn model(&self) -> &'static Model;
    /// Unit vectors of `model().dims` floats; adds the "passage: " prefix.
    fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

impl Embed for Embedder {
    fn model(&self) -> &'static Model {
        self.model
    }
    fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_passages(texts)
    }
}

/// Little-endian f32, exactly `dims` of them.
pub fn to_blob(v: &[f32], dims: usize) -> Result<Vec<u8>> {
    if v.len() != dims {
        return Err(Error::Format(format!("vector of {} dimensions, expected {dims}", v.len())));
    }
    Ok(v.iter().flat_map(|x| x.to_le_bytes()).collect())
}

pub fn from_blob(b: &[u8], dims: usize) -> Result<Vec<f32>> {
    if b.len() != dims * 4 {
        return Err(Error::Format(format!("vector of {} bytes, expected {}", b.len(), dims * 4)));
    }
    Ok(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// Persisted embedding scope (meta `embed_scope`). Default: everything.
/// Bounds are absolute FILETIMEs on `received` (since inclusive, until
/// exclusive); the typed input is kept for display only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scope {
    pub since: Option<i64>,
    pub since_input: Option<String>,
    pub until: Option<i64>,
    pub until_input: Option<String>,
    /// `-F` specs, matched like `search -F` at each run.
    pub folders: Vec<String>,
}

pub fn scope(c: &Connection) -> Result<Scope> {
    match get_meta(c, "embed_scope")? {
        Some(s) => serde_json::from_str(&s).map_err(|e| Error::Format(format!("stored embedding scope: {e}"))),
        None => Ok(Scope::default()),
    }
}

/// Store the scope. Folder specs are checked once folders are indexed.
/// Vectors that fall out of scope are kept.
pub fn set_scope(c: &Connection, s: &Scope) -> Result<()> {
    if matches!((s.since, s.until), (Some(a), Some(b)) if a >= b) {
        return Err(Error::Usage("--embed-since must be before --embed-until".into()));
    }
    let known: i64 = c.query_row("SELECT COUNT(*) FROM folders", [], |r| r.get(0)).map_err(db_err)?;
    if known > 0 {
        folder_ids(c, &s.folders)?;
    }
    set_meta(c, "embed_scope", &serde_json::to_string(s).map_err(|e| Error::Io(e.to_string()))?)
}

/// Folder ids of the scope as JSON for `IN_SCOPE` ?3. A spec matching no
/// folder (deleted, not synced yet) selects nothing, not everything.
fn scope_folders(c: &Connection, s: &Scope) -> Result<String> {
    if s.folders.is_empty() {
        return Ok("[]".into());
    }
    let mut ids = vec![-1]; // never a NID
    for f in &s.folders {
        match folder_ids(c, std::slice::from_ref(f)) {
            Ok(v) => ids.extend(v),
            Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(serde_json::to_string(&ids).unwrap())
}

/// Chunk `c` of message `m` is in scope; ?1 since, ?2 until, ?3 folder ids.
const IN_SCOPE: &str = "((?1 IS NULL OR m.received >= ?1) AND (?2 IS NULL OR m.received < ?2)
    AND (?3 = '[]' OR EXISTS (SELECT 1 FROM locations l WHERE l.message = m.id
                              AND l.folder IN (SELECT value FROM json_each(?3)))))";

/// Model of the stored vectors (meta), if any.
pub fn current_model(c: &Connection) -> Result<Option<String>> {
    get_meta(c, "embed_model")
}

/// Refuse `m` while vectors of another model (or revision) exist, unless `reembed`.
pub fn check_model(c: &Connection, m: &Model, reembed: bool) -> Result<()> {
    if reembed {
        return Ok(());
    }
    let other: Option<(String, i64)> = c
        .query_row(
            "SELECT model, COUNT(*) FROM vectors WHERE NOT (model = ?1 AND revision = ?2) GROUP BY model LIMIT 1",
            [m.name, m.revision],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(db_err)?;
    match other {
        Some((name, n)) => Err(Error::Usage(format!(
            "the index holds {n} vectors of {name}; pass --reembed to replace them with {} vectors",
            m.name
        ))),
        None => Ok(()),
    }
}

pub struct Options {
    /// Passages per model call and per transaction.
    pub batch: usize,
    /// Start no new batch past this budget.
    pub max_time: Option<Duration>,
    /// Drop the vectors of any other model first.
    pub reembed: bool,
}

/// A batch: (chunk id, passage) rows.
type Rows = Vec<(i64, String)>;

/// Embed the pending in-scope chunks, newest message first: passage =
/// subject + newline + chunk text. Each of `es` (a worker) embeds one
/// batch at a time, in parallel; this thread is the only DB writer and
/// commits each batch in its own transaction as it completes, so commits
/// may interleave slightly out of newest-first order. An interrupted run loses at most the in-flight
/// batches and resumes. A failed batch stores nothing; the run stops
/// dispatching, commits the batches still in flight and returns the first
/// error. Counts only.
pub fn run<E: Embed + ?Sized>(c: &mut Connection, es: &mut [&mut E], o: &Options) -> Result<Json> {
    let start = Instant::now();
    let m = es.first().ok_or_else(|| Error::Usage("no embedding worker".into()))?.model();
    check_model(c, m, o.reembed)?;
    let tx = c.transaction().map_err(db_err)?;
    let dropped = tx
        .execute("DELETE FROM vectors WHERE NOT (model = ?1 AND revision = ?2)", [m.name, m.revision])
        .map_err(db_err)?;
    set_meta(&tx, "embed_model", m.name)?;
    set_meta(&tx, "embed_revision", m.revision)?;
    tx.commit().map_err(db_err)?;

    let s = scope(c)?;
    let sql = format!(
        "SELECT c.id FROM chunks c JOIN messages m ON m.id = c.message
         WHERE {IN_SCOPE} AND NOT EXISTS (SELECT 1 FROM vectors v WHERE v.chunk = c.id AND v.model = ?4)
         ORDER BY m.received DESC, m.id DESC, c.seq"
    );
    let ids: Vec<i64> = c
        .prepare(&sql)
        .map_err(db_err)?
        .query_map(params![s.since, s.until, scope_folders(c, &s)?, m.name], |r| r.get(0))
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;

    let workers = es.len();
    let t = Instant::now();
    let (mut embedded, mut interrupted) = (0u64, false);
    let (job_tx, job_rx) = mpsc::channel::<Rows>();
    let job_rx = Mutex::new(job_rx);
    std::thread::scope(|sc| -> Result<()> {
        // Dropped on return, closing the channel: the workers stop.
        let job_tx = job_tx;
        let (res_tx, res_rx) = mpsc::channel::<(Rows, Result<Vec<Vec<f32>>>)>();
        for e in es.iter_mut() {
            let (job_rx, res_tx) = (&job_rx, res_tx.clone());
            sc.spawn(move || {
                loop {
                    // Lock released before embedding; ends when the channel closes.
                    let Ok(rows) = job_rx.lock().unwrap().recv() else { break };
                    let texts: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
                    let r = guarded(|| e.passages(&texts));
                    if res_tx.send((rows, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(res_tx);
        let (mut groups, mut in_flight, mut sent, mut failed) = (ids.chunks(o.batch.max(1)), 0, 0, None);
        loop {
            while failed.is_none() && !interrupted && in_flight < workers {
                let Some(group) = groups.next() else { break };
                // The first batch always runs.
                interrupted = sent > 0 && o.max_time.is_some_and(|max| start.elapsed() > max);
                if interrupted {
                    break;
                }
                job_tx.send(texts(c, group)?).map_err(|_| Error::Format("embedding worker stopped".into()))?;
                (in_flight, sent) = (in_flight + 1, sent + 1);
            }
            if in_flight == 0 {
                break;
            }
            let (rows, r) = res_rx.recv().map_err(|_| Error::Format("embedding worker stopped".into()))?;
            in_flight -= 1;
            match r.and_then(|vecs| store(c, m, &rows, &vecs)) {
                Ok(()) => embedded += rows.len() as u64,
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        failed.map_or(Ok(()), Err)
    })?;
    let secs = t.elapsed().as_secs_f64();
    let pending = counts(c)?["pending"].as_u64().unwrap_or(0);
    Ok(json!({
        "model": m.name,
        "revision": m.revision,
        "dims": m.dims,
        "batch": o.batch,
        "workers": workers,
        "embedded": embedded,
        "dropped": dropped,
        "pending": pending,
        "complete": pending == 0,
        "interrupted": interrupted,
        "passages_per_s": if secs > 0.0 { (embedded as f64 / secs * 100.0).round() / 100.0 } else { 0.0 },
        "elapsed_ms": start.elapsed().as_millis() as u64,
    }))
}

/// Passages of the chunks still present.
fn texts(c: &Connection, ids: &[i64]) -> Result<Rows> {
    let mut rows = Vec::with_capacity(ids.len());
    for &id in ids {
        let text = c
            .query_row(
                "SELECT COALESCE(m.subject, '') || char(10) || c.text FROM chunks c
                 JOIN messages m ON m.id = c.message WHERE c.id = ?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(db_err)?;
        rows.extend(text.map(|t| (id, t)));
    }
    Ok(rows)
}

/// One batch, one transaction: all its vectors or none.
fn store(c: &mut Connection, m: &Model, rows: &Rows, vecs: &[Vec<f32>]) -> Result<()> {
    if vecs.len() != rows.len() {
        return Err(Error::Format(format!("{} vectors for {} passages", vecs.len(), rows.len())));
    }
    let tx = c.transaction().map_err(db_err)?;
    {
        let mut ins = tx
            .prepare_cached("INSERT OR REPLACE INTO vectors(chunk, model, revision, vec) VALUES (?1, ?2, ?3, ?4)")
            .map_err(db_err)?;
        for ((id, _), v) in rows.iter().zip(vecs) {
            ins.execute(params![id, m.name, m.revision, to_blob(v, m.dims)?]).map_err(db_err)?;
        }
    }
    tx.commit().map_err(db_err)
}

/// Embedding counts and scope summary for `status` and `scope`. No names:
/// folder specs are counted, not printed.
pub fn counts(c: &Connection) -> Result<Json> {
    let s = scope(c)?;
    let (name, rev) = (current_model(c)?, get_meta(c, "embed_revision")?);
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(s), 0), COALESCE(SUM(s AND e), 0), COALESCE(SUM(e AND NOT s), 0)
         FROM (SELECT {IN_SCOPE} AS s,
                      EXISTS (SELECT 1 FROM vectors v WHERE v.chunk = c.id AND v.model = ?4 AND v.revision = ?5) AS e
               FROM chunks c JOIN messages m ON m.id = c.message)"
    );
    let (total, in_scope, embedded, out_embedded): (i64, i64, i64, i64) = c
        .query_row(&sql, params![s.since, s.until, scope_folders(c, &s)?, name, rev], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(db_err)?;
    let vectors: i64 = c.query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0)).map_err(db_err)?;
    let t = |ft: Option<i64>| ft.and_then(|t| format_ft(t, false));
    Ok(json!({
        "model": name,
        "revision": rev,
        "dims": name.as_deref().and_then(|n| model::get(n).ok()).map(|m| m.dims),
        "vectors": vectors,
        "chunks_in_scope": in_scope,
        "embedded": embedded,
        "pending": in_scope - embedded,
        // Rounded down: 100 only when nothing is pending.
        "embedded_pct": (in_scope > 0).then(|| (embedded * 1000 / in_scope) as f64 / 10.0),
        "out_of_scope": total - in_scope,
        "out_of_scope_embedded": out_embedded,
        "scope": {
            "all": s == Scope::default(),
            "since": t(s.since),
            "since_input": s.since_input,
            "until": t(s.until),
            "until_input": s.until_input,
            "folders": s.folders.len(),
        },
    }))
}

/// Below-normal CPU priority for this process; true when in effect. Call
/// before the compute threads start (on Linux they inherit it).
#[cfg(unix)]
pub fn lower_priority() -> bool {
    unsafe extern "C" {
        fn getpriority(which: i32, who: u32) -> i32;
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }
    // SAFETY: plain syscalls on this process (PRIO_PROCESS = 0, who 0 = self).
    // An already nicer process is left alone.
    unsafe { getpriority(0, 0) >= 10 || setpriority(0, 0, 10) == 0 }
}

#[cfg(windows)]
pub fn lower_priority() -> bool {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn SetPriorityClass(process: *mut c_void, class: u32) -> i32;
    }
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
    // SAFETY: pseudo-handle of this process, documented priority class.
    unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) != 0 }
}

#[cfg(not(any(unix, windows)))]
pub fn lower_priority() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::hash::{DefaultHasher, Hash, Hasher};

    use super::*;
    use crate::index::migrate;
    use crate::index::model::MODELS;
    use crate::index::sync::fake::*;
    use crate::index::sync::{self, sync};

    /// How the fake fails on a passage containing its marker.
    #[derive(Clone, Copy)]
    enum Fail {
        Err,
        /// One float short: refused inside the batch transaction.
        Dims,
        Panic,
    }

    /// Deterministic fake: hashed text -> vector. Records what it embeds.
    struct Hashed {
        model: &'static Model,
        dims: usize,
        seen: Vec<String>,
        fail: Option<(&'static str, Fail)>,
    }

    fn hashed(m: usize) -> Hashed {
        Hashed { model: &MODELS[m], dims: MODELS[m].dims, seen: Vec::new(), fail: None }
    }

    /// `run` with one worker.
    fn run1(c: &mut Connection, e: &mut Hashed, o: &Options) -> Result<Json> {
        run(c, &mut [e], o)
    }

    /// `run` with `n` workers like `e`; returns what each one embedded.
    fn run_n(c: &mut Connection, n: usize, e: &Hashed, o: &Options) -> (Result<Json>, Vec<Vec<String>>) {
        let mut es: Vec<Hashed> = (0..n).map(|_| Hashed { seen: Vec::new(), ..*e }).collect();
        let r = run(c, &mut es.iter_mut().collect::<Vec<_>>(), o);
        (r, es.into_iter().map(|e| e.seen).collect())
    }

    impl Embed for Hashed {
        fn model(&self) -> &'static Model {
            self.model
        }
        fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.seen.extend(texts.iter().map(|t| t.to_string()));
            let fail = self.fail.filter(|(m, _)| texts.iter().any(|t| t.contains(m))).map(|f| f.1);
            match fail {
                Some(Fail::Err) => return Err(Error::Format("fake failure".into())),
                Some(Fail::Panic) => panic!("fake panic"),
                _ => {}
            }
            Ok(texts
                .iter()
                .map(|t| {
                    let mut h = DefaultHasher::new();
                    t.hash(&mut h);
                    let mut x = h.finish() | 1;
                    let short = matches!(fail, Some(Fail::Dims)) && self.fail.is_some_and(|(m, _)| t.contains(m));
                    (0..self.dims - short as usize)
                        .map(|_| {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            (x % 2001) as f32 / 1000.0 - 1.0
                        })
                        .collect()
                })
                .collect())
        }
    }

    fn indexed(f: &mut Fake) -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        sync(&mut c, f, &sync::Options::default()).unwrap();
        c
    }

    fn opts(batch: usize) -> Options {
        Options { batch, max_time: None, reembed: false }
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// received of fake message `nid`.
    fn at(nid: i64) -> i64 {
        134_000_000_000_000_000 + nid * 10_000_000
    }

    #[test]
    fn blob_round_trip_and_dims() {
        let v = vec![0.5f32, -1.25, f32::MIN_POSITIVE, 3.0];
        let b = to_blob(&v, 4).unwrap();
        assert_eq!(b.len(), 16);
        assert_eq!(&b[..4], &0.5f32.to_le_bytes());
        assert_eq!(from_blob(&b, 4).unwrap(), v);
        assert_eq!(to_blob(&v, 3).unwrap_err().kind(), "format");
        assert!(from_blob(&b[..15], 4).is_err() && from_blob(&b, 3).is_err());
        // A model returning the wrong size: error, nothing stored.
        let mut f = mailbox();
        let mut c = indexed(&mut f);
        let mut e = Hashed { dims: 383, ..hashed(0) };
        assert!(run1(&mut c, &mut e, &opts(8)).is_err());
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vectors"), 0);
    }

    #[test]
    fn newest_first_with_subject() {
        let mut f = mailbox();
        let mut c = indexed(&mut f);
        let mut e = hashed(0);
        let s = run1(&mut c, &mut e, &opts(2)).unwrap();
        assert_eq!((s["embedded"].as_u64(), s["pending"].as_u64()), (Some(3), Some(0)), "{s}");
        assert_eq!((s["complete"].as_bool(), s["dims"].as_u64()), (Some(true), Some(384)), "{s}");
        let firsts: Vec<&str> = e.seen.iter().map(|t| t.split('\n').next().unwrap()).collect();
        assert_eq!(firsts, ["Old note", "Lunch", "Budget café"], "newest message first");
        assert_eq!(e.seen[1], "Lunch\nLunch on friday at noon.");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vectors WHERE length(vec) = 384 * 4 AND model = 'e5-small'"), 3);
        assert_eq!(current_model(&c).unwrap().as_deref(), Some("e5-small"));
        // Nothing pending: a second run embeds nothing.
        assert_eq!(run1(&mut c, &mut e, &opts(2)).unwrap()["embedded"], 0);
        assert_eq!(e.seen.len(), 3);
    }

    #[test]
    fn scope_by_dates_and_folders() {
        let mut f = mailbox();
        let mut c = indexed(&mut f);
        let set = |c: &Connection, s: Scope| set_scope(c, &s).unwrap();
        let n = |c: &Connection| counts(c).unwrap()["chunks_in_scope"].as_i64().unwrap();
        assert_eq!(n(&c), 3);
        set(&c, Scope { since: Some(at(0x224)), since_input: Some("x".into()), ..Scope::default() });
        assert_eq!(n(&c), 2, "since is inclusive");
        set(&c, Scope { until: Some(at(0x224)), ..Scope::default() });
        assert_eq!(n(&c), 1, "until is exclusive");
        set(&c, Scope { folders: vec!["inbox".into()], ..Scope::default() });
        assert_eq!(n(&c), 2);
        set(&c, Scope { folders: vec!["Archive".into()], ..Scope::default() });
        assert_eq!(n(&c), 1, "path with subfolders");
        let inbox_late = Scope { since: Some(at(0x224)), folders: vec!["Inbox".into()], ..Scope::default() };
        set(&c, inbox_late);
        assert_eq!(n(&c), 1);
        let mut e = hashed(0);
        assert_eq!(run1(&mut c, &mut e, &opts(8)).unwrap()["embedded"], 1);
        assert_eq!(e.seen, ["Lunch\nLunch on friday at noon."]);

        // Errors: unknown folder, empty range; the stored scope is unchanged.
        let before = scope(&c).unwrap();
        let bad = Scope { folders: vec!["nope".into()], ..Scope::default() };
        assert_eq!(set_scope(&c, &bad).unwrap_err().kind(), "not_found");
        let bad = Scope { since: Some(at(9)), until: Some(at(9)), ..Scope::default() };
        assert_eq!(set_scope(&c, &bad).unwrap_err().kind(), "usage");
        assert_eq!(scope(&c).unwrap(), before);

        // A folder that disappears selects nothing (not everything).
        f.folders.retain(|x| x.nid != 0x22);
        f.folders.push(folder(0x62, "Other"));
        for e in f.entries.iter_mut().filter(|e| e.folder == 0x22) {
            e.folder = 0x62;
        }
        f.revision += 1;
        sync(&mut c, &mut f, &sync::Options::default()).unwrap();
        assert_eq!(n(&c), 0);
    }

    #[test]
    fn out_of_scope_vectors_kept_and_counted() {
        let mut f = mailbox();
        let mut c = indexed(&mut f);
        set_scope(&c, &Scope { folders: vec!["Inbox".into()], ..Scope::default() }).unwrap();
        run1(&mut c, &mut hashed(0), &opts(8)).unwrap();
        set_scope(&c, &Scope { folders: vec!["Archive".into()], ..Scope::default() }).unwrap();
        let v = counts(&c).unwrap();
        assert_eq!((v["vectors"].as_i64(), v["out_of_scope_embedded"].as_i64()), (Some(2), Some(2)), "{v}");
        assert_eq!((v["chunks_in_scope"].as_i64(), v["pending"].as_i64()), (Some(1), Some(1)), "{v}");
        assert_eq!((v["out_of_scope"].as_i64(), v["embedded_pct"].as_f64()), (Some(2), Some(0.0)), "{v}");
        run1(&mut c, &mut hashed(0), &opts(8)).unwrap();
        set_scope(&c, &Scope::default()).unwrap();
        let v = counts(&c).unwrap();
        assert_eq!((v["embedded"].as_i64(), v["embedded_pct"].as_f64()), (Some(3), Some(100.0)), "{v}");
        assert_eq!(v["scope"]["all"], true);
    }

    fn bulk() -> Fake {
        let mut f = mailbox();
        for i in 0..11u32 {
            let body = format!("Bulk text {i}.\n\n{}", "lorem ipsum ".repeat(if i % 3 == 0 { 300 } else { 5 }));
            f.add(0x1004 + i * 0x20, 0x22, Some(&format!("<b{i}@fict.invalid>")), "Bulk", &body);
        }
        f
    }

    fn vectors(c: &Connection) -> Vec<(i64, String, Vec<u8>)> {
        c.prepare("SELECT chunk, model, vec FROM vectors ORDER BY chunk")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Interrupted runs with 1 or 3 workers resume to the 1-worker state.
    #[test]
    fn resume_reaches_the_same_state() {
        let mut f1 = bulk();
        let mut one = indexed(&mut f1);
        let total = count(&one, "SELECT COUNT(*) FROM chunks");
        assert!(total > 14, "{total}");
        run1(&mut one, &mut hashed(0), &opts(3)).unwrap();
        for workers in [1, 3] {
            let mut f2 = bulk();
            let mut many = indexed(&mut f2);
            let o = Options { max_time: Some(Duration::ZERO), ..opts(3) };
            let mut runs = 0;
            loop {
                let s = run_n(&mut many, workers, &hashed(0), &o).0.unwrap();
                runs += 1;
                assert_eq!(s["workers"], workers);
                if s["complete"] == true {
                    break;
                }
                assert_eq!((s["embedded"].as_i64(), s["interrupted"].as_bool()), (Some(3), Some(true)), "{s}");
            }
            assert_eq!(runs, (total + 2) / 3, "{workers} workers");
            assert_eq!(vectors(&one), vectors(&many), "{workers} workers");
        }
        assert_eq!(vectors(&one).len() as i64, total);
    }

    /// N workers store exactly the vectors of one worker, whatever the batch.
    #[test]
    fn workers_store_the_same_vectors() {
        let mut f = bulk();
        let mut one = indexed(&mut f);
        run1(&mut one, &mut hashed(0), &opts(2)).unwrap();
        for (workers, batch) in [(2, 2), (3, 1), (4, 5), (8, 2)] {
            let mut f = bulk();
            let mut c = indexed(&mut f);
            let (s, seen) = run_n(&mut c, workers, &hashed(0), &opts(batch));
            let s = s.unwrap();
            assert_eq!((s["workers"].as_u64(), s["complete"].as_bool()), (Some(workers as u64), Some(true)), "{s}");
            assert_eq!(vectors(&one), vectors(&c), "{workers}x{batch}");
            assert_eq!(seen.iter().map(Vec::len).sum::<usize>(), vectors(&c).len(), "each passage embedded once");
        }
    }

    /// A failing worker (error, wrong vector, panic): the run returns the
    /// error, every batch is stored whole or not at all, a rerun completes.
    #[test]
    fn worker_failure_keeps_batches_whole() {
        let mut f = bulk();
        let mut reference = indexed(&mut f);
        run1(&mut reference, &mut hashed(0), &opts(2)).unwrap();
        // Chunk ids in commit-independent embedding order, as batches of 2.
        let order: Vec<i64> = reference
            .prepare("SELECT c.id FROM chunks c JOIN messages m ON m.id = c.message ORDER BY m.received DESC, m.id DESC, c.seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for fail in [Fail::Err, Fail::Dims, Fail::Panic] {
            let mut f = bulk();
            let mut c = indexed(&mut f);
            // With Dims the error comes inside the batch transaction.
            let marker = "Bulk text 4.";
            let e = Hashed { fail: Some((marker, fail)), ..hashed(0) };
            assert!(run_n(&mut c, 3, &e, &opts(2)).0.is_err());
            let stored: std::collections::HashSet<i64> = vectors(&c).iter().map(|v| v.0).collect();
            let mut failed = 0;
            for group in order.chunks(2) {
                let n = group.iter().filter(|id| stored.contains(id)).count();
                assert!(n == 0 || n == group.len(), "partial batch {group:?}");
                let text: Vec<String> = group
                    .iter()
                    .map(|id| c.query_row("SELECT text FROM chunks WHERE id = ?1", [id], |r| r.get(0)).unwrap())
                    .collect();
                if text.iter().any(|t| t.contains(marker)) {
                    assert_eq!(n, 0, "failed batch not stored");
                    failed += 1;
                }
            }
            assert_eq!(failed, 1);
            run_n(&mut c, 3, &hashed(0), &opts(2)).0.unwrap();
            assert_eq!(vectors(&c), vectors(&reference));
        }
    }

    #[test]
    fn deleted_message_drops_its_vectors() {
        let mut f = bulk();
        let mut c = indexed(&mut f);
        run1(&mut c, &mut hashed(0), &opts(8)).unwrap();
        let before = count(&c, "SELECT COUNT(*) FROM vectors");
        let gone = count(
            &c,
            "SELECT COUNT(*) FROM chunks c JOIN messages m ON m.id = c.message WHERE m.ident = 'mid:<b0@fict.invalid>'",
        );
        assert!(gone > 1, "{gone}");
        f.remove(0x1004);
        sync(&mut c, &mut f, &sync::Options::default()).unwrap();
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vectors"), before - gone);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vectors WHERE chunk NOT IN (SELECT id FROM chunks)"), 0);
    }

    #[test]
    fn model_switch_needs_reembed() {
        let mut f = mailbox();
        let mut c = indexed(&mut f);
        run1(&mut c, &mut hashed(0), &opts(8)).unwrap();
        let before = vectors(&c);
        let e = run1(&mut c, &mut hashed(1), &opts(8)).unwrap_err();
        assert_eq!(e.kind(), "usage");
        assert!(e.to_string().contains("--reembed") && e.to_string().contains("e5-small"), "{e}");
        assert_eq!(vectors(&c), before, "refused: nothing changed");
        assert_eq!(current_model(&c).unwrap().as_deref(), Some("e5-small"));
        let s = run1(&mut c, &mut hashed(1), &Options { reembed: true, ..opts(8) }).unwrap();
        assert_eq!((s["dropped"].as_u64(), s["embedded"].as_u64()), (Some(3), Some(3)), "{s}");
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vectors WHERE model = 'e5-base' AND length(vec) = 768 * 4"), 3);
        assert_eq!(count(&c, "SELECT COUNT(*) FROM vectors"), 3);
        let v = counts(&c).unwrap();
        assert_eq!((v["model"].as_str(), v["dims"].as_u64()), (Some("e5-base"), Some(768)));
        check_model(&c, &MODELS[1], false).unwrap();
        assert_eq!(check_model(&c, &MODELS[0], false).unwrap_err().kind(), "usage");
    }

    #[test]
    fn outputs_have_counts_only() {
        let mut f = mailbox();
        f.folders[0].path = "SECRET_FOLDER".into();
        f.folders[0].name = "SECRET_FOLDER".into();
        f.add(0x2e4, 0x22, Some("<SECRET@fict.invalid>"), "SECRET_SUBJECT", "SECRET_BODY");
        let mut c = indexed(&mut f);
        let s = Scope {
            since: Some(at(0x204)),
            since_input: Some("2026-01-01".into()),
            folders: vec!["secret_folder".into()],
            ..Scope::default()
        };
        set_scope(&c, &s).unwrap();
        let outputs = [
            run1(&mut c, &mut hashed(0), &opts(2)).unwrap().to_string(),
            counts(&c).unwrap().to_string(),
            crate::index::search::status(&c).unwrap().to_string(),
        ];
        for o in &outputs {
            for m in ["SECRET", "secret", "Zed", "fict", "Budget", "Lunch", "Archive"] {
                assert!(!o.contains(m), "output leaks {m:?}: {o}");
            }
        }
        assert!(outputs[1].contains("\"folders\":1"), "{}", outputs[1]);
    }

    /// Real e5-small on ~200 synthetic chunks: passages/s and DB bytes per
    /// 1,000 vectors. E5_SMALL_DIR=... cargo test --release --features index
    /// real_e5_small -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_e5_small() {
        let dir =
            std::env::var_os("E5_SMALL_DIR").map(Into::into).unwrap_or_else(|| model::default_dir(&MODELS[0]).unwrap());
        let mut f = Fake { folders: vec![folder(0x22, "Inbox")], ..Fake::default() };
        for (i, p) in crate::index::embed::synthetic_passages(200).iter().enumerate() {
            f.add(0x1004 + i as u32 * 0x20, 0x22, Some(&format!("<s{i}@fict.invalid>")), "Synthetic", p);
        }
        let mut c = indexed(&mut f);
        let bytes =
            |c: &Connection| count(c, "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()");
        let before = bytes(&c);
        let mut e = Embedder::load(&MODELS[0], &dir).unwrap();
        let s = run(&mut c, &mut [&mut e], &opts(crate::index::embed::DEFAULT_BATCH)).unwrap();
        let n = s["embedded"].as_i64().unwrap();
        assert_eq!(n, count(&c, "SELECT COUNT(*) FROM chunks"));
        eprintln!(
            "{}",
            json!({
                "chunks": n,
                "passages_per_s": s["passages_per_s"],
                "elapsed_ms": s["elapsed_ms"],
                "db_bytes_per_1000_vectors": (bytes(&c) - before) * 1000 / n,
                "threads": std::env::var("RAYON_NUM_THREADS").ok(),
            })
        );
    }
}
