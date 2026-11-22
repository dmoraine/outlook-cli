//! `outlook-index search`: keyword (FTS5), semantic (brute-force cosine over
//! the stored vectors) and hybrid (RRF of both), plus `status`.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value as Json, json};

use super::db_err;
use super::model::{self, Model};
use super::sync::{get_meta, totals};
use super::vectors::current_model;
use crate::error::{Error, Result};
use crate::store::MSGFLAG_READ;
use crate::timeutil::format_ft;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every word required (FTS5).
    #[default]
    Keyword,
    /// Nearest chunks by meaning.
    Semantic,
    /// RRF of an any-word keyword search and the semantic search.
    Hybrid,
}

#[derive(Default)]
pub struct Query {
    pub text: String,
    pub limit: usize,
    /// Folder names or paths with subfolders (case-insensitive); any.
    pub folders: Vec<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Sender name or address contains; any.
    pub from: Vec<String>,
    /// To/Cc display text contains; any.
    pub to: Vec<String>,
    pub has_attachments: bool,
    /// Excerpt length in characters; 0 = EXCERPT_CHARS.
    pub excerpt_max: usize,
    pub local_time: bool,
    pub mode: Mode,
    /// Semantic and hybrid: the query vector ("query: " prefix) and its model.
    pub vector: Option<Vec<f32>>,
    pub model: Option<&'static Model>,
}

/// FTS5 expression for free text: every word becomes a quoted phrase, so
/// operators, column filters and quotes in the input are plain text.
/// None when no word has a letter or digit.
pub fn fts_query(q: &str) -> Option<String> {
    let w = words(q);
    (!w.is_empty()).then(|| w.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "))
}

/// Words with a letter or digit. NUL ends an FTS5 string: control
/// characters become spaces.
fn words(q: &str) -> Vec<String> {
    let q = q.replace(char::is_control, " ");
    q.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).map(str::to_string).collect()
}

fn quote(w: &str) -> String {
    format!("\"{}\"", w.replace('"', "\"\""))
}

/// FTS5 any-word expression: every word, quoted, joined by OR.
fn or_query(w: &[String]) -> String {
    w.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" OR ")
}

/// The query has a word to search for (any mode).
pub fn check_text(q: &str) -> Result<()> {
    if words(q).is_empty() { Err(Error::Usage("the query has no word to search for".into())) } else { Ok(()) }
}

/// Default and maximum excerpt length (characters).
pub const EXCERPT_CHARS: usize = 300;
pub const EXCERPT_MAX: usize = 2000;
/// Messages taken from each hybrid leg before fusion, whatever --limit.
pub const LEG: usize = 200;
/// Reciprocal Rank Fusion constant.
pub const RRF_K: f64 = 60.0;

/// Metadata filters on message `m`: ?2 since, ?3 until, ?4 folder ids (JSON), ?5 attachments only.
const FILTER: &str = "(?2 IS NULL OR m.received >= ?2)
      AND (?3 IS NULL OR m.received < ?3)
      AND (?4 = '[]' OR EXISTS (SELECT 1 FROM locations l WHERE l.message = m.id
                                AND l.folder IN (SELECT value FROM json_each(?4))))
      AND (?5 = 0 OR m.has_attachments = 1)";

/// Folder ids for -F specs, case-insensitive: a name, or a path with its
/// subfolders ("Inbox" matches "Inbox/Projects", not "Inboxes").
pub(super) fn folder_ids(c: &Connection, specs: &[String]) -> Result<Vec<i64>> {
    let all: Vec<(i64, String, String)> = c
        .prepare("SELECT nid, path, name FROM folders")
        .map_err(db_err)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(db_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_err)?;
    let mut out = Vec::new();
    for spec in specs {
        let s = spec.trim().trim_matches('/').to_lowercase();
        let n = out.len();
        let under = |p: &str| p.strip_prefix(s.as_str()).is_some_and(|r| r.is_empty() || r.starts_with('/'));
        out.extend(all.iter().filter(|f| under(&f.1.to_lowercase()) || f.2.to_lowercase() == s).map(|f| f.0));
        if out.len() == n {
            return Err(Error::NotFound(format!("folder '{}' in the index (run `outlook-index sync`)", spec.trim())));
        }
    }
    Ok(out)
}

/// Mode to run, with the model of its vectors and a fallback warning.
/// `requested` None = hybrid when vectors of the index model exist, else
/// keyword. Semantic or hybrid without such vectors: keyword plus a warning.
/// `model` other than the model of the stored vectors: usage error.
/// Messages name models only.
pub fn plan(
    c: &Connection,
    requested: Option<Mode>,
    model: Option<&str>,
) -> Result<(Mode, Option<&'static Model>, Option<String>)> {
    if requested == Some(Mode::Keyword) {
        return Ok((Mode::Keyword, None, None));
    }
    let stored = current_model(c)?;
    let has = |name: &str, rev: Option<&str>| {
        c.query_row(
            "SELECT 1 FROM vectors WHERE model = ?1 AND (?2 IS NULL OR revision = ?2) LIMIT 1",
            params![name, rev],
            |_| Ok(()),
        )
        .optional()
        .map(|o| o.is_some())
        .map_err(db_err)
    };
    if let (Some(want), Some(s)) = (model, stored.as_deref())
        && want != s
        && has(s, None)?
    {
        return Err(Error::Usage(format!(
            "the index vectors are {s}; search with --model {s}, or embed again with --model {want} --reembed"
        )));
    }
    let m = model::choose(model, stored.as_deref())?;
    if has(m.name, Some(m.revision))? {
        return Ok((requested.unwrap_or(Mode::Hybrid), Some(m), None));
    }
    let warn = requested.map(|_| format!("no {} vectors in the index; keyword search used", m.name));
    Ok((Mode::Keyword, None, warn))
}

/// Best chunk of a message in one leg.
struct Leg {
    message: i64,
    chunk: i64,
    score: f64,
    /// Keyword snippet; else the chunk start.
    excerpt: Option<String>,
}

/// Sender/recipient filters (case-insensitive contains, any), checked in Rust.
struct People {
    from: Vec<String>,
    to: Vec<String>,
}

impl People {
    fn new(q: &Query) -> People {
        let low = |v: &[String]| v.iter().map(|s| s.to_lowercase()).collect();
        People { from: low(&q.from), to: low(&q.to) }
    }

    /// Row columns from `i`: from_name, from_email, sender, display_to, display_cc.
    fn ok(&self, r: &rusqlite::Row, i: usize) -> Result<bool> {
        if self.from.is_empty() && self.to.is_empty() {
            return Ok(true);
        }
        let s = |k| r.get::<_, Option<String>>(i + k).map_err(db_err);
        let any = |want: &[String], hay: &[Option<String>]| {
            let hay = hay.iter().flatten().map(String::as_str).collect::<Vec<_>>().join(" ").to_lowercase();
            want.is_empty() || want.iter().any(|w| hay.contains(w.as_str()))
        };
        Ok(any(&self.from, &[s(0)?, s(1)?, s(2)?]) && any(&self.to, &[s(3)?, s(4)?]))
    }
}

const PEOPLE: &str = "m.from_name, m.from_email, m.sender, m.display_to, m.display_cc";

/// Keyword leg: FTS5 `expr`, best chunk per message by bm25, at most `limit` messages.
// ponytail: walks every matching chunk to group; fine for one mailbox, add a
// per-message MIN(bm25) pass if very common words get slow.
fn keyword_leg(c: &Connection, q: &Query, expr: &str, folders: &str, limit: usize) -> Result<Vec<Leg>> {
    // snippet() counts tokens (FTS5 caps at 64); the char bound is applied after.
    let tokens = (excerpt_max(q) / 6).clamp(4, 64) as i64;
    let sql = format!(
        "SELECT m.id, c.id, bm25(chunks_fts), {PEOPLE}
         FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid JOIN messages m ON m.id = c.message
         WHERE chunks_fts MATCH ?1 AND {FILTER}
         ORDER BY bm25(chunks_fts)"
    );
    let people = People::new(q);
    let mut st = c.prepare(&sql).map_err(db_err)?;
    let mut rows = st.query(params![expr, q.since, q.until, folders, q.has_attachments]).map_err(db_err)?;
    let (mut seen, mut out) = (HashSet::new(), Vec::new());
    while out.len() < limit {
        let Some(r) = rows.next().map_err(db_err)? else { break };
        let message: i64 = r.get(0).map_err(db_err)?;
        if seen.contains(&message) || !people.ok(r, 3)? {
            continue;
        }
        seen.insert(message);
        out.push(Leg {
            message,
            chunk: r.get(1).map_err(db_err)?,
            score: -r.get::<_, f64>(2).map_err(db_err)?,
            excerpt: None,
        });
    }
    // Snippets for the kept chunks only (costly on every match).
    let mut st = c
        .prepare(
            "SELECT snippet(chunks_fts, 1, '', '', '...', ?3) FROM chunks_fts WHERE chunks_fts MATCH ?1 AND rowid = ?2",
        )
        .map_err(db_err)?;
    for l in &mut out {
        l.excerpt = Some(st.query_row(params![expr, l.chunk, tokens], |r| r.get(0)).map_err(db_err)?);
    }
    Ok(out)
}

/// A message's best chunk, ordered by score then lower message id.
#[derive(PartialEq)]
struct Best(f32, i64, i64);

impl Eq for Best {}

impl Ord for Best {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0).then(o.1.cmp(&self.1))
    }
}

impl PartialOrd for Best {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// Vectors of ?1 at revision ?6 in message order, with the FILTER params.
fn semantic_sql() -> String {
    format!(
        "SELECT c.message, c.id, v.vec, {PEOPLE}
         FROM chunks c INDEXED BY chunks_message
         JOIN vectors v ON v.chunk = c.id AND v.model = ?1 AND v.revision = ?6
         JOIN messages m ON m.id = c.message
         WHERE {FILTER}
         ORDER BY c.message"
    )
}

/// Semantic leg: streams the vectors of `m` in message order (no sort, no
/// load of all vectors), cosine = dot product of unit vectors, best chunk
/// per message, keeps the `limit` best messages in a min-heap.
// ponytail: brute force, O(vectors) per query; an ANN index is deferred.
fn semantic_leg(c: &Connection, q: &Query, m: &Model, v: &[f32], folders: &str, limit: usize) -> Result<Vec<Leg>> {
    if v.len() != m.dims {
        return Err(Error::Format(format!("query vector of {} dimensions, expected {}", v.len(), m.dims)));
    }
    let people = People::new(q);
    let mut st = c.prepare(&semantic_sql()).map_err(db_err)?;
    let mut rows =
        st.query(params![m.name, q.since, q.until, folders, q.has_attachments, m.revision]).map_err(db_err)?;
    let mut heap = BinaryHeap::with_capacity(limit + 1);
    let mut push = |b: Option<Best>| {
        if let Some(b) = b {
            heap.push(Reverse(b));
            if heap.len() > limit {
                heap.pop();
            }
        }
    };
    let (mut cur, mut ok, mut best) = (None, false, None::<Best>);
    while let Some(r) = rows.next().map_err(db_err)? {
        let message: i64 = r.get(0).map_err(db_err)?;
        if cur != Some(message) {
            push(best.take());
            cur = Some(message);
            ok = people.ok(r, 3)?;
        }
        if !ok {
            continue;
        }
        let blob = r.get_ref(2).map_err(db_err)?.as_blob().map_err(|e| Error::Format(e.to_string()))?;
        if blob.len() != m.dims * 4 {
            return Err(Error::Format(format!("stored vector of {} bytes, expected {}", blob.len(), m.dims * 4)));
        }
        let s: f32 = blob.as_chunks::<4>().0.iter().zip(v).map(|(b, x)| f32::from_le_bytes(*b) * x).sum();
        let b = Best(s, message, r.get(1).map_err(db_err)?);
        if best.as_ref().is_none_or(|x| b.0 > x.0) {
            best = Some(b);
        }
    }
    push(best);
    let mut out = heap.into_sorted_vec();
    out.truncate(limit);
    Ok(out.into_iter().map(|Reverse(b)| Leg { message: b.1, chunk: b.2, score: b.0 as f64, excerpt: None }).collect())
}

/// Reciprocal Rank Fusion of ranked message lists: sum of 1 / (k + rank).
/// Returns (message, score, rank in each list), best first; ties by message id.
pub fn rrf(lists: &[&[i64]]) -> Vec<(i64, f64, Vec<Option<usize>>)> {
    let mut acc: HashMap<i64, (f64, Vec<Option<usize>>)> = HashMap::new();
    for (l, list) in lists.iter().enumerate() {
        for (i, &id) in list.iter().enumerate() {
            let e = acc.entry(id).or_insert_with(|| (0.0, vec![None; lists.len()]));
            e.0 += 1.0 / (RRF_K + (i + 1) as f64);
            e.1[l] = Some(i + 1);
        }
    }
    let mut out: Vec<_> = acc.into_iter().map(|(id, (s, r))| (id, s, r)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

fn excerpt_max(q: &Query) -> usize {
    if q.excerpt_max == 0 { EXCERPT_CHARS } else { q.excerpt_max.min(EXCERPT_MAX) }
}

fn round(x: f64, digits: i32) -> f64 {
    let p = 10f64.powi(digits);
    (x * p).round() / p
}

/// One hit: message fields, current location (preferring a filtered
/// folder), bounded excerpt of its best chunk.
fn hit(c: &Connection, q: &Query, folders: &str, leg: &Leg, ranks: Json, scores: Json) -> Result<Json> {
    let excerpt = match &leg.excerpt {
        Some(e) => e.clone(),
        // The chunk without the overlap it shares with the previous one.
        None => {
            let (text, overlap): (String, Option<i64>) = c
                .query_row(
                    "SELECT c.text, (SELECT p.end_char FROM chunks p WHERE p.message = c.message
                                     AND p.seq = c.seq - 1) - c.start_char
                     FROM chunks c WHERE c.id = ?1",
                    [leg.chunk],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(db_err)?;
            let rest: String = text.chars().skip(overlap.unwrap_or(0).max(0) as usize).collect();
            if rest.trim().is_empty() { text } else { rest.trim_start().to_string() }
        }
    };
    c.query_row(
        "SELECT l.nid, m.message_id, f.path, m.received, m.sent, m.subject, m.from_name, m.sender, m.from_email,
                m.display_to, m.display_cc, m.size, m.class, m.has_attachments, l.flags
         FROM messages m
         JOIN locations l ON l.nid = (SELECT x.nid FROM locations x WHERE x.message = m.id
                ORDER BY (x.folder IN (SELECT value FROM json_each(?2))) DESC, x.nid LIMIT 1)
         LEFT JOIN folders f ON f.nid = l.folder
         WHERE m.id = ?1",
        params![leg.message, folders],
        |r| {
            let s = |i| r.get::<_, Option<String>>(i);
            let time = |i| r.get::<_, Option<i64>>(i).map(|t| t.and_then(|t| format_ft(t, q.local_time)));
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "message_id": s(1)?,
                "folder": s(2)?,
                "received": time(3)?,
                "sent": time(4)?,
                "subject": s(5)?,
                "from": { "name": s(6)?.or(s(7)?), "email": s(8)? },
                "to": s(9)?,
                "cc": s(10)?,
                "size": r.get::<_, Option<i64>>(11)?,
                "class": s(12)?,
                "has_attachments": r.get::<_, bool>(13)?,
                "read": r.get::<_, i64>(14)? & MSGFLAG_READ != 0,
                "excerpt": crate::body::truncate(excerpt, excerpt_max(q)).0,
                "source": "body",
                "ranks": ranks,
                "scores": scores,
            }))
        },
    )
    .map_err(db_err)
}

/// Best matching chunk per message, best first, in `q.mode`. Keyword: every
/// word. Semantic: nearest chunks to `q.vector`. Hybrid: RRF (k = 60) of an
/// any-word keyword leg and the semantic leg, top LEG of each.
pub fn search(c: &Connection, q: &Query) -> Result<Vec<Json>> {
    check_text(&q.text)?;
    let words = words(&q.text);
    let folders = serde_json::to_string(&folder_ids(c, &q.folders)?).unwrap();
    let limit = q.limit.max(1);
    let vector = || match (q.model, q.vector.as_deref()) {
        (Some(m), Some(v)) => Ok((m, v)),
        _ => Err(Error::Usage("semantic and hybrid search need a query vector and its model".into())),
    };
    match q.mode {
        Mode::Keyword => keyword_leg(c, q, &fts_query(&q.text).unwrap(), &folders, limit)?
            .iter()
            .enumerate()
            .map(|(i, l)| hit(c, q, &folders, l, json!({ "keyword": i + 1 }), json!({ "keyword": round(l.score, 4) })))
            .collect(),
        Mode::Semantic => {
            let (m, v) = vector()?;
            semantic_leg(c, q, m, v, &folders, limit)?
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    hit(c, q, &folders, l, json!({ "semantic": i + 1 }), json!({ "semantic": round(l.score, 4) }))
                })
                .collect()
        }
        Mode::Hybrid => {
            let (m, v) = vector()?;
            let kw = keyword_leg(c, q, &or_query(&words), &folders, LEG)?;
            let sem = semantic_leg(c, q, m, v, &folders, LEG)?;
            let ids = |l: &[Leg]| l.iter().map(|x| x.message).collect::<Vec<_>>();
            let fused = rrf(&[&ids(&kw), &ids(&sem)]);
            let mut out = Vec::new();
            for (i, (_, score, r)) in fused.iter().take(limit).enumerate() {
                let (k, s) = (r[0].map(|i| &kw[i - 1]), r[1].map(|i| &sem[i - 1]));
                let (mut ranks, mut scores) = (json!({ "hybrid": i + 1 }), json!({ "rrf": round(*score, 6) }));
                for (name, leg, rank) in [("keyword", k, r[0]), ("semantic", s, r[1])] {
                    if let (Some(l), Some(rank)) = (leg, rank) {
                        ranks[name] = json!(rank);
                        scores[name] = json!(round(l.score, 4));
                    }
                }
                // The keyword snippet shows the words; else the semantic chunk.
                let mut h = hit(c, q, &folders, k.or(s).unwrap(), ranks, scores)?;
                // Flat contract fields: null rank = absent from that leg.
                h["keyword_rank"] = json!(r[0]);
                h["semantic_rank"] = json!(r[1]);
                h["score"] = json!(round(*score, 6));
                out.push(h);
            }
            Ok(out)
        }
    }
}

/// Aggregate counts only.
pub fn status(c: &Connection) -> Result<Json> {
    let at = get_meta(c, "sync_at")?.and_then(|t| t.parse().ok()).and_then(|t| format_ft(t, false));
    let last = get_meta(c, "sync_last")?.and_then(|s| serde_json::from_str::<Json>(&s).ok());
    let mut v = totals(c)?;
    let o = v.as_object_mut().unwrap();
    o.insert("schema".into(), json!(super::schema_version(c)?));
    o.insert("embedding".into(), super::vectors::counts(c)?);
    o.insert("include".into(), json!(super::sync::include(c)?.unwrap_or_default()));
    let bytes = c.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| {
        r.get::<_, i64>(0)
    });
    o.insert("db_bytes".into(), json!(bytes.map_err(db_err)?));
    o.insert(
        "last_sync".into(),
        json!({
            "at": at,
            "revision": get_meta(c, "sync_revision")?,
            "complete": get_meta(c, "sync_complete")?.as_deref() == Some("1"),
            "summary": last.map(|mut l| { l.as_object_mut().map(|o| o.remove("totals")); l }),
        }),
    );
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::migrate;
    use crate::index::sync::fake::*;
    use crate::index::sync::{Options, sync};

    fn indexed() -> (Connection, Fake) {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        let mut f = mailbox();
        f.add(0x324, 0x22, Some("<e@fict.invalid>"), "Empty kickoff", "");
        f.add(
            0x304,
            0x42,
            Some("<c1@fict.invalid>"),
            "Report",
            "Quarterly report draft.\n\nThe budget numbers follow.",
        )
        .sender = Some("Ann Fict".into());
        sync(&mut c, &mut f, &Options::default()).unwrap();
        (c, f)
    }

    fn q(text: &str) -> Query {
        Query { text: text.into(), limit: 10, ..Query::default() }
    }

    fn ids(c: &Connection, q: &Query) -> Vec<u64> {
        search(c, q).unwrap().iter().map(|h| h["id"].as_u64().unwrap()).collect()
    }

    #[test]
    fn accent_insensitive() {
        let (c, _) = indexed();
        assert_eq!(ids(&c, &q("cafe")), [0x204]);
        assert_eq!(ids(&c, &q("CAFÉ")), [0x204]);
        assert_eq!(ids(&c, &q("resume")), [0x244]);
        assert_eq!(ids(&c, &q("valide budget")), [0x204], "all words required");
        assert_eq!(ids(&c, &q("empty")), [0x324], "subject of an empty body");
        assert_eq!(ids(&c, &q("old note")), [0x244], "subject is searched");
        let h = &search(&c, &q("lunch")).unwrap()[0];
        assert_eq!(h["folder"], "Inbox");
        assert_eq!(h["message_id"], "<a2@fict.invalid>");
        assert_eq!(h["source"], "body");
        assert_eq!(h["subject"], "Lunch");
        assert_eq!(h["from"]["email"], "zed@fict.invalid");
        assert!(h["excerpt"].as_str().unwrap().contains("Lunch"));
        assert!(h["received"].as_str().unwrap().ends_with('Z'));
        assert_eq!(h["ranks"]["keyword"], 1);
    }

    #[test]
    fn hostile_queries() {
        let (c, _) = indexed();
        for s in [
            "\"",
            "\"\"",
            "budget\"",
            "budget OR",
            "NOT budget",
            "budget AND",
            "NEAR(budget",
            "text:budget",
            "budget*",
            "*",
            "^budget",
            "(budget",
            "budget)",
            "-budget",
            "{text}: budget",
            "'; DROP TABLE messages; --",
            "a\u{0}b",
            "budget ++ --",
            "\\",
            "%",
            "_",
        ] {
            let r = search(&c, &q(s));
            match fts_query(s) {
                Some(_) => assert!(r.is_ok(), "{s:?}: {:?}", r.err()),
                None => assert_eq!(r.unwrap_err().kind(), "usage", "{s:?}"),
            }
        }
        assert_eq!(fts_query("a \"b\" c:d"), Some("\"a\" \"\"\"b\"\"\" \"c:d\"".into()));
        assert_eq!(fts_query("  *  ( ) "), None);
        assert_eq!(ids(&c, &q("text:budget")), Vec::<u64>::new(), "no column filter");
        assert_eq!(ids(&c, &q("budget OR")), Vec::<u64>::new(), "OR is a word, not an operator");
        assert_eq!(count(&c), 5, "still intact");
    }

    fn count(c: &Connection) -> i64 {
        c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn grouped_and_filtered() {
        let (c, _) = indexed();
        let mut all = ids(&c, &q("budget"));
        all.sort();
        assert_eq!(all, [0x204, 0x304], "one hit per message");
        let with = |f: &dyn Fn(&mut Query)| {
            let mut x = q("budget");
            f(&mut x);
            ids(&c, &x)
        };
        assert_eq!(with(&|x| x.folders = vec!["archive/2026".into()]), [0x304]);
        assert_eq!(with(&|x| x.folders = vec!["2026".into()]), [0x304], "by name");
        assert_eq!(with(&|x| x.folders = vec!["inbox".into(), "archive/2026".into()]).len(), 2);
        let mut x = q("budget");
        x.folders = vec!["nope".into()];
        assert_eq!(search(&c, &x).unwrap_err().kind(), "not_found");
        assert_eq!(with(&|x| x.from = vec!["ann".into()]), [0x304]);
        assert_eq!(with(&|x| x.from = vec!["%".into()]), Vec::<u64>::new(), "LIKE wildcards are literal");
        assert_eq!(with(&|x| x.to = vec!["ann f".into()]).len(), 2);
        let t = 134_000_000_000_000_000 + 0x204 * 10_000_000;
        assert_eq!(with(&|x| x.since = Some(t + 1)), [0x304]);
        assert_eq!(with(&|x| x.until = Some(t + 1)), [0x204]);
        assert_eq!(
            with(&|x| {
                x.since = Some(t + 1);
                x.folders = vec!["Inbox".into()]
            }),
            Vec::<u64>::new()
        );
        assert_eq!(with(&|x| x.limit = 1).len(), 1);
    }

    #[test]
    fn folder_prefix_on_slash_boundaries() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        let mut f = mailbox();
        f.folders.extend([folder(0x62, "Inbox/Projects/Alpha"), folder(0x82, "Inboxes")]);
        f.add(0x604, 0x62, Some("<p@fict.invalid>"), "Plan", "Budget plan alpha.");
        f.add(0x624, 0x82, Some("<q@fict.invalid>"), "Other", "Budget other box.");
        sync(&mut c, &mut f, &Options::default()).unwrap();
        let by = |spec: &str| {
            let mut v = ids(&c, &Query { folders: vec![spec.into()], ..q("budget") });
            v.sort();
            v
        };
        assert_eq!(by("Inbox"), [0x204, 0x604], "subfolders, not Inboxes");
        assert_eq!(by("inbox/"), [0x204, 0x604]);
        assert_eq!(by("INBOX/projects"), [0x604], "case-insensitive prefix");
        assert_eq!(by("Inboxes"), [0x624]);
        assert_eq!(by("alpha"), [0x604], "by name");
        let x = Query { folders: vec!["Inbox/Proj".into()], ..q("budget") };
        assert_eq!(search(&c, &x).unwrap_err().kind(), "not_found", "not a / boundary");
    }

    #[test]
    fn excerpt_bounded() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        let mut f = mailbox();
        f.add(
            0x404,
            0x22,
            Some("<d@fict.invalid>"),
            "Long",
            &format!("{} needle {}", "x ".repeat(3000), "y ".repeat(3000)),
        );
        sync(&mut c, &mut f, &Options::default()).unwrap();
        let h = &search(&c, &q("needle")).unwrap()[0];
        let e = h["excerpt"].as_str().unwrap();
        assert!(e.contains("needle") && e.chars().count() <= 400, "{e}");
    }

    #[test]
    fn hit_fields_filters_and_moves() {
        let (mut c, mut f) = indexed();
        let h = &search(&c, &q("lunch")).unwrap()[0];
        assert_eq!((h["to"].as_str(), h["cc"].as_str()), (Some("Ann Fict"), Some("Cy Fict")));
        assert_eq!(
            (h["class"].as_str(), h["read"].as_bool(), h["has_attachments"].as_bool()),
            (Some("IPM.Note"), Some(true), Some(false))
        );
        assert!(h["sent"].as_str().is_some_and(|t| t.ends_with('Z')) && h["size"].as_i64().unwrap() > 1000, "{h}");
        assert_eq!(ids(&c, &Query { has_attachments: true, ..q("budget") }), Vec::<u64>::new());
        assert_eq!(ids(&c, &Query { to: vec!["cy f".into()], ..q("lunch") }), [0x224], "--to matches Cc");
        // Attachment flag from the contents row, read flag change and a move.
        f.add(0x504, 0x22, Some("<g@fict.invalid>"), "Invoice", "Invoice attached.").flags = 0x10;
        f.relocate(0x224, 0x524, 0x42);
        sync(&mut c, &mut f, &Options::default()).unwrap();
        assert_eq!(ids(&c, &Query { has_attachments: true, ..q("invoice") }), [0x504]);
        let h = &search(&c, &q("invoice")).unwrap()[0];
        assert_eq!((h["has_attachments"].as_bool(), h["read"].as_bool()), (Some(true), Some(false)));
        let h = &search(&c, &q("lunch")).unwrap()[0];
        assert_eq!(
            (h["id"].as_u64(), h["folder"].as_str()),
            (Some(0x524), Some("Archive/2026")),
            "current NID and path"
        );
        // Excerpt length is bounded by excerpt_max.
        let long = format!("{} needle {}", "word ".repeat(400), "word ".repeat(400));
        f.add(0x544, 0x22, Some("<h@fict.invalid>"), "Long", &long);
        sync(&mut c, &mut f, &Options::default()).unwrap();
        for (max, cap) in [(40, 40), (0, EXCERPT_CHARS), (100_000, EXCERPT_MAX)] {
            let e = search(&c, &Query { excerpt_max: max, ..q("needle") }).unwrap()[0]["excerpt"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(e.chars().count() <= cap && !e.is_empty(), "{max}: {e}");
        }
    }

    #[test]
    fn status_counts_only() {
        let (c, _) = indexed();
        let s = status(&c).unwrap();
        assert_eq!(s["messages"], 5);
        assert_eq!(s["locations"], 5);
        assert_eq!(s["folders"], 2);
        assert!(s["chunks"].as_u64().unwrap() >= 4);
        assert_eq!(
            (s["embedding"]["vectors"].as_u64(), s["embedding"]["pending"].as_u64()),
            (Some(0), s["chunks"].as_u64()),
            "{s}"
        );
        assert_eq!(s["embedding"]["scope"]["all"], true, "{s}");
        assert_eq!(s["last_sync"]["complete"], true);
        assert_eq!(s["schema"], crate::index::schema_version(&c).unwrap());
        assert!(s["last_sync"]["revision"].is_string() && s["last_sync"]["at"].is_string(), "{s}");
        assert_eq!(s["last_sync"]["summary"]["new"], 5, "{s}");
        let t = s.to_string();
        for m in ["Zed", "Ann", "fict", "Budget", "budget", "Inbox", "Archive", "Lunch", "kickoff"] {
            assert!(!t.contains(m), "status leaks {m:?}: {t}");
        }
    }

    // --- semantic and hybrid ---

    use crate::index::model::MODELS;
    use crate::index::vectors::{self, Embed};

    /// Fake meaning: FR/EN synonyms share a dimension, other words hash to
    /// the rest. Same function for queries and passages.
    struct Concepts;

    const SYNONYMS: &[&[&str]] = &[&["invoice", "facture", "factures"], &["meeting", "réunion"], &["budget"]];

    fn meaning(t: &str) -> Vec<f32> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut v = vec![0f32; 384];
        for w in t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
            let w = w.to_lowercase();
            let d = SYNONYMS.iter().position(|s| s.contains(&w.as_str())).unwrap_or_else(|| {
                let mut h = DefaultHasher::new();
                w.hash(&mut h);
                10 + (h.finish() % 374) as usize
            });
            v[d] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    impl Embed for Concepts {
        fn model(&self) -> &'static Model {
            &MODELS[0]
        }
        fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| meaning(t)).collect())
        }
    }

    fn embed(c: &mut Connection) {
        let o = vectors::Options { batch: 4, max_time: None, reembed: false };
        vectors::run(c, &mut [&mut Concepts, &mut Concepts], &o).unwrap();
    }

    /// indexed() plus FR/EN invoices and a two-chunk message; not embedded yet.
    fn more() -> (Connection, Fake) {
        let (mut c, mut f) = indexed();
        f.add(0x404, 0x22, Some("<f1@fict.invalid>"), "Facture mars", "La facture de mars est jointe.").flags = 0x10;
        f.add(0x424, 0x42, Some("<f2@fict.invalid>"), "Invoice", "Invoice for the March delivery.");
        let long = format!("{}\n\nLes factures du trimestre sont en retard.", "lorem ipsum dolor. ".repeat(90));
        f.add(0x444, 0x22, Some("<f3@fict.invalid>"), "Notes", &long);
        sync(&mut c, &mut f, &Options::default()).unwrap();
        (c, f)
    }

    fn sem(text: &str, mode: Mode) -> Query {
        Query { mode, vector: Some(meaning(text)), model: Some(&MODELS[0]), ..q(text) }
    }

    #[test]
    fn semantic_ranks_by_meaning() {
        let (mut c, _) = more();
        embed(&mut c);
        assert_eq!(ids(&c, &q("invoice")), [0x424], "keyword: the word only");
        let hits = search(&c, &sem("invoice", Mode::Semantic)).unwrap();
        let got: Vec<u64> = hits.iter().map(|h| h["id"].as_u64().unwrap()).collect();
        assert_eq!(got[..3], [0x424, 0x404, 0x444], "FR synonyms found, best first");
        assert_eq!(got.len(), 8, "one hit per message: {got:?}");
        let h = &hits[2];
        let e = h["excerpt"].as_str().unwrap();
        assert!(e.starts_with("Les factures"), "best chunk, without the overlap: {h}");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM chunks WHERE text LIKE '%factures%'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert_eq!((h["ranks"]["semantic"].as_u64(), h["source"].as_str()), (Some(3), Some("body")));
        assert!(h["ranks"].get("keyword").is_none(), "{h}");
        let s: Vec<f64> = hits.iter().map(|h| h["scores"]["semantic"].as_f64().unwrap()).collect();
        assert!(s.windows(2).all(|w| w[0] >= w[1]) && s[0] <= 1.0 && s[0] > 0.6, "{s:?}");
        // Same fields as a keyword hit.
        let k = &search(&c, &q("invoice")).unwrap()[0];
        let keys = |h: &Json| h.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&hits[0]), keys(k));
        assert_eq!(hits[0]["message_id"], "<f2@fict.invalid>");
        for (max, cap) in [(10, 10), (0, EXCERPT_CHARS)] {
            let e = search(&c, &Query { excerpt_max: max, ..sem("lorem", Mode::Semantic) }).unwrap()[0]["excerpt"]
                .as_str()
                .unwrap()
                .chars()
                .count();
            assert!(e > 0 && e <= cap, "{max}: {e}");
        }
        // A query vector is required, of the model's size.
        assert_eq!(search(&c, &Query { mode: Mode::Semantic, ..q("invoice") }).unwrap_err().kind(), "usage");
        let bad = Query { vector: Some(vec![0.0; 3]), ..sem("invoice", Mode::Hybrid) };
        assert_eq!(search(&c, &bad).unwrap_err().kind(), "format");
    }

    #[test]
    fn semantic_filters() {
        let (mut c, _) = more();
        embed(&mut c);
        let with = |f: &dyn Fn(&mut Query)| {
            let mut x = sem("invoice", Mode::Semantic);
            f(&mut x);
            let mut v = ids(&c, &x);
            v.truncate(3);
            v
        };
        let t = |nid: i64| 134_000_000_000_000_000 + nid * 10_000_000;
        assert_eq!(with(&|x| x.folders = vec!["inbox".into()])[..2], [0x404, 0x444]);
        assert_eq!(with(&|x| x.since = Some(t(0x424))), [0x424, 0x444]);
        assert_eq!(with(&|x| x.until = Some(t(0x424)))[0], 0x404);
        assert_eq!(with(&|x| x.from = vec!["ann".into()]), [0x304]);
        assert_eq!(with(&|x| x.to = vec!["nobody".into()]), Vec::<u64>::new());
        assert_eq!(with(&|x| x.to = vec!["cy f".into()]), [0x424, 0x404, 0x444]);
        assert_eq!(with(&|x| x.has_attachments = true), [0x404]);
        assert_eq!(with(&|x| x.limit = 1), [0x424]);
        let x = Query { folders: vec!["nope".into()], ..sem("invoice", Mode::Semantic) };
        assert_eq!(search(&c, &x).unwrap_err().kind(), "not_found");
        // Hybrid: the same filters on both legs.
        let mut x = sem("invoice budget", Mode::Hybrid);
        x.folders = vec!["Archive".into()];
        let mut v = ids(&c, &x);
        v.sort();
        assert_eq!(v, [0x244, 0x304, 0x424]);
        // A hit's id is its location in the filtered folder.
        let h = &search(&c, &Query { has_attachments: true, ..sem("invoice", Mode::Hybrid) }).unwrap();
        assert_eq!((h.len(), h[0]["folder"].as_str()), (1, Some("Inbox")));
    }

    #[test]
    fn rrf_fuses_by_rank() {
        let f = rrf(&[&[1, 2, 3], &[3, 4]]);
        let order: Vec<i64> = f.iter().map(|x| x.0).collect();
        assert_eq!(order, [3, 1, 2, 4], "in both lists first; ties by id");
        assert_eq!(f[0].2, [Some(3), Some(1)]);
        assert!((f[0].1 - (1.0 / 63.0 + 1.0 / 61.0)).abs() < 1e-12);
        assert_eq!((f[3].1, &f[3].2), (1.0 / 62.0, &vec![None, Some(2)]));
        assert!(rrf(&[&[], &[]]).is_empty());
    }

    #[test]
    fn hybrid_or_leg_keyword_and() {
        let (mut c, _) = more();
        embed(&mut c);
        assert_eq!(ids(&c, &q("budget lunch")), Vec::<u64>::new(), "keyword: every word");
        let hits = search(&c, &sem("budget lunch", Mode::Hybrid)).unwrap();
        let has = |nid: u64| hits.iter().find(|h| h["id"] == nid).unwrap_or_else(|| panic!("{nid:#x}"));
        for nid in [0x204, 0x224, 0x304] {
            assert!(has(nid)["ranks"]["keyword"].is_u64(), "any word in hybrid: {}", has(nid));
        }
        // Every word joins the any-word leg, quoted.
        let w = |t: &str| or_query(&words(t));
        assert_eq!(w("The lunch, de l'équipe"), "\"The\" OR \"lunch,\" OR \"de\" OR \"l'équipe\"");
        assert_eq!(w("a\"b OR NEAR(x"), "\"a\"\"b\" OR \"OR\" OR \"NEAR(x\"");
        let hits = search(&c, &sem("the lunch", Mode::Hybrid)).unwrap();
        assert!(hits.iter().any(|h| h["id"] == 0x304 && h["keyword_rank"].is_u64()), "\"the\" matches: {hits:?}");
        assert!(search(&c, &sem("lunch\" OR * NEAR(", Mode::Hybrid)).is_ok());
        let hits = search(&c, &sem("invoice budget", Mode::Hybrid)).unwrap();
        for (i, h) in hits.iter().enumerate() {
            assert_eq!(h["ranks"]["hybrid"], i + 1);
            let rr = |k: &str| h["ranks"][k].as_f64().map_or(0.0, |r| 1.0 / (RRF_K + r));
            let want = rr("keyword") + rr("semantic");
            assert!((h["scores"]["rrf"].as_f64().unwrap() - want).abs() < 2e-6, "{h}");
            assert_eq!(h["ranks"]["keyword"].is_u64(), h["scores"]["keyword"].is_f64());
            assert_eq!(h["ranks"]["semantic"].is_u64(), h["scores"]["semantic"].is_f64());
            // Flat fields: numeric, or JSON null when the leg misses.
            assert_eq!(
                h["keyword_rank"],
                if h["ranks"]["keyword"].is_u64() { h["ranks"]["keyword"].clone() } else { Json::Null }
            );
            assert_eq!(
                h["semantic_rank"],
                if h["ranks"]["semantic"].is_u64() { h["ranks"]["semantic"].clone() } else { Json::Null }
            );
            assert!(h.as_object().unwrap().contains_key("keyword_rank") && h["score"].is_f64(), "{h}");
            assert_eq!(h["score"], h["scores"]["rrf"]);
        }
        let rrf: Vec<f64> = hits.iter().map(|h| h["scores"]["rrf"].as_f64().unwrap()).collect();
        assert!(rrf.windows(2).all(|w| w[0] >= w[1]), "{rrf:?}");
        let h = hits.iter().find(|h| h["id"] == 0x404).unwrap();
        assert!(h["ranks"].get("keyword").is_none() && h["ranks"]["semantic"].is_u64(), "semantic only: {h}");
        assert!(h["keyword_rank"].is_null() && h["semantic_rank"].is_u64(), "{h}");
        assert!((h["score"].as_f64().unwrap() - 1.0 / (RRF_K + h["semantic_rank"].as_f64().unwrap())).abs() < 2e-6);
        assert!(h["excerpt"].as_str().unwrap().contains("facture"), "{h}");
        let h = hits.iter().find(|h| h["id"] == 0x424).unwrap();
        assert!(h["ranks"]["keyword"].is_u64() && h["ranks"]["semantic"] == 1, "both legs: {h}");
        // Keyword only: outside the embedding scope (no vector).
        c.execute("DELETE FROM vectors WHERE chunk IN (SELECT id FROM chunks WHERE message = (SELECT message FROM locations WHERE nid = 0x304))", []).unwrap();
        let hits = search(&c, &sem("invoice budget", Mode::Hybrid)).unwrap();
        let h = hits.iter().find(|h| h["id"] == 0x304).unwrap();
        assert!(h["keyword_rank"].is_u64() && h["semantic_rank"].is_null(), "keyword only: {h}");
        assert!((h["score"].as_f64().unwrap() - 1.0 / (RRF_K + h["keyword_rank"].as_f64().unwrap())).abs() < 2e-6);
        assert_eq!(search(&c, &Query { limit: 2, ..sem("invoice budget", Mode::Hybrid) }).unwrap().len(), 2);
        assert_eq!(search(&c, &sem("\" * (", Mode::Hybrid)).unwrap_err().kind(), "usage");
    }

    #[test]
    fn hybrid_legs_capped() {
        let (mut c, mut f) = indexed();
        for i in 0..250u32 {
            f.add(0x1000 + i * 0x20, 0x22, None, "Invoice", &format!("Invoice number {i}."));
        }
        sync(&mut c, &mut f, &Options::default()).unwrap();
        embed(&mut c);
        let hits = search(&c, &Query { limit: 1000, ..sem("invoice", Mode::Hybrid) }).unwrap();
        let leg = |k: &str| hits.iter().filter_map(|h| h[k].as_u64()).collect::<Vec<_>>();
        for k in ["keyword_rank", "semantic_rank"] {
            let r = leg(k);
            assert_eq!((r.len(), r.iter().max()), (LEG, Some(&(LEG as u64))), "{k}");
        }
        assert!(hits.len() >= LEG && hits.len() <= 2 * LEG, "union of the legs: {}", hits.len());
        assert!(hits.iter().all(|h| !h["keyword_rank"].is_null() || !h["semantic_rank"].is_null()));
    }

    #[test]
    fn keyword_unchanged_by_vectors() {
        let (mut c, _) = more();
        let runs = |c: &Connection| {
            ["budget", "invoice", "lunch", "factures", "budget lunch"]
                .map(|t| serde_json::to_string(&search(c, &q(t)).unwrap()).unwrap())
        };
        let before = runs(&c);
        embed(&mut c);
        assert_eq!(runs(&c), before);
        let h = &search(&c, &q("budget")).unwrap()[0];
        assert_eq!(h["ranks"].as_object().unwrap().len(), 1, "{h}");
    }

    type Plan = (Mode, Option<&'static str>, Option<String>);

    fn plan_of(c: &Connection, m: Option<Mode>, model: Option<&str>) -> Result<Plan> {
        plan(c, m, model).map(|(a, b, w)| (a, b.map(|b| b.name), w))
    }

    #[test]
    fn plan_default_fallback_mismatch() {
        let (mut c, mut f) = more();
        f.add(0x464, 0x22, Some("<SECRET@fict.invalid>"), "SECRET_SUBJECT", "SECRET_BODY invoice");
        sync(&mut c, &mut f, &Options::default()).unwrap();
        let p = |m, model| plan_of(&c, m, model);
        assert_eq!(p(None, None).unwrap(), (Mode::Keyword, None, None), "no vectors: keyword, quietly");
        assert_eq!(p(Some(Mode::Keyword), None).unwrap(), (Mode::Keyword, None, None));
        let mut texts = Vec::new();
        for m in [Mode::Semantic, Mode::Hybrid] {
            let (mode, model, w) = p(Some(m), None).unwrap();
            let w = w.expect("warning");
            assert!((mode, model.is_none()) == (Mode::Keyword, true) && w.contains("e5-base"), "new index: {w}");
            texts.push(w);
        }
        let (_, _, w) = p(Some(Mode::Hybrid), Some("e5-base")).unwrap();
        assert!(w.unwrap().contains("e5-base"), "no vectors at all: no mismatch");

        embed(&mut c);
        let p = |m, model| plan_of(&c, m, model);
        let small = Some("e5-small");
        assert_eq!(p(None, None).unwrap(), (Mode::Hybrid, small, None), "vectors: hybrid by default");
        assert_eq!(p(Some(Mode::Semantic), Some("e5-small")).unwrap(), (Mode::Semantic, small, None));
        assert_eq!(p(Some(Mode::Keyword), Some("e5-base")).unwrap(), (Mode::Keyword, None, None));
        for m in [None, Some(Mode::Semantic), Some(Mode::Hybrid)] {
            let e = p(m, Some("e5-base")).unwrap_err();
            let t = e.to_string();
            assert!(e.kind() == "usage" && t.contains("e5-small") && t.contains("--reembed"), "{t}");
            texts.push(t);
        }
        assert_eq!(p(None, Some("e5-large")).unwrap_err().kind(), "usage");
        // Vectors of another revision are not the current model's.
        c.execute("UPDATE vectors SET revision = 'old'", []).unwrap();
        let p = |m, model| plan_of(&c, m, model);
        assert_eq!(p(None, None).unwrap(), (Mode::Keyword, None, None));
        texts.push(p(Some(Mode::Semantic), None).unwrap().2.unwrap());
        for t in &texts {
            for m in ["SECRET", "invoice", "Inbox", "fict", "Zed"] {
                assert!(!t.contains(m), "leaks {m:?}: {t}");
            }
        }
    }

    #[test]
    fn semantic_scan_needs_no_sort() {
        let (c, _) = more();
        let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {}", semantic_sql())).unwrap();
        let plan: Vec<String> = st
            .query_map(params!["e5-small", None::<i64>, None::<i64>, "[]", false, "r"], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!plan.iter().any(|p| p.contains("TEMP B-TREE")), "{plan:?}");
    }

    /// Synthetic index of N vectors (dim 384, 2 chunks per message): semantic
    /// and hybrid query times, no model. cargo test --release --features
    /// index semantic_bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn semantic_bench() {
        use std::time::Instant;
        let m = &MODELS[0];
        let dir = std::env::temp_dir().join(format!("outlook-index-bench-{}", std::process::id()));
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let words = ["budget", "meeting", "invoice", "projet", "rapport", "équipe", "delivery", "contrat"];
        for n in [50_000usize, 200_000] {
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("bench-{n}.sqlite"));
            let _ = std::fs::remove_file(&p);
            let mut c = crate::index::open(&p).unwrap();
            let t = Instant::now();
            let tx = c.transaction().unwrap();
            tx.execute("INSERT INTO folders VALUES (34, 'Inbox', 'Inbox'), (66, 'Archive', 'Archive')", []).unwrap();
            let unit = |rnd: &mut dyn FnMut() -> u64| {
                let v: Vec<f32> = (0..m.dims).map(|_| (rnd() % 2001) as f32 / 1000.0 - 1.0).collect();
                let s = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.iter().map(|x| x / s).collect::<Vec<f32>>()
            };
            for i in 0..n as i64 / 2 {
                tx.execute(
                    "INSERT INTO messages(id, ident, received, has_attachments, subject, sender)
                     VALUES (?1, 'mid:' || ?1, ?2, ?3, 'Synthetic', 'Fict')",
                    params![i + 1, 134_000_000_000_000_000 + i * 10_000_000, i % 7 == 0],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO locations VALUES (?1, ?2, ?3, 1)",
                    params![i * 32 + 4, 34 + 32 * (i % 2), i + 1],
                )
                .unwrap();
                for seq in 0..2 {
                    let text: Vec<&str> = (0..40).map(|_| words[(rnd() % 8) as usize]).collect();
                    tx.execute(
                        "INSERT INTO chunks(message, seq, text) VALUES (?1, ?2, ?3)",
                        params![i + 1, seq, text.join(" ")],
                    )
                    .unwrap();
                    let blob = vectors::to_blob(&unit(&mut rnd), m.dims).unwrap();
                    tx.execute(
                        "INSERT INTO vectors VALUES (last_insert_rowid(), ?1, ?2, ?3)",
                        params![m.name, m.revision, blob],
                    )
                    .unwrap();
                }
            }
            crate::index::sync::set_meta(&tx, "embed_model", m.name).unwrap();
            crate::index::sync::set_meta(&tx, "embed_revision", m.revision).unwrap();
            tx.commit().unwrap();
            let build_s = t.elapsed().as_secs_f64();
            drop(c);
            let c = crate::index::open(&p).unwrap();
            let v = unit(&mut rnd);
            let query = |mode, text: &str, f: &dyn Fn(&mut Query)| {
                let mut x = Query { mode, vector: Some(v.clone()), model: Some(m), ..q(text) };
                f(&mut x);
                let t = Instant::now();
                assert_eq!(search(&c, &x).unwrap().len(), 10);
                t.elapsed().as_secs_f64() * 1000.0
            };
            let first = query(Mode::Semantic, "x", &|_| {});
            let med = |f: &dyn Fn() -> f64| {
                let mut v: Vec<f64> = (0..5).map(|_| f()).collect();
                v.sort_by(f64::total_cmp);
                (v[2] * 10.0).round() / 10.0
            };
            let semantic = med(&|| query(Mode::Semantic, "x", &|_| {}));
            let filtered = med(&|| {
                query(Mode::Semantic, "x", &|x| {
                    x.folders = vec!["Inbox".into()];
                    x.since = Some(134_000_000_000_000_000)
                })
            });
            let hybrid = med(&|| query(Mode::Hybrid, "budget rapport", &|_| {}));
            let keyword = med(&|| query(Mode::Keyword, "budget rapport", &|_| {}));
            let bytes: i64 = c
                .query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| {
                    r.get(0)
                })
                .unwrap();
            eprintln!(
                "{}",
                json!({
                    "vectors": n, "dims": m.dims, "build_s": (build_s * 10.0).round() / 10.0, "db_bytes": bytes,
                    "semantic_first_ms": (first * 10.0).round() / 10.0, "semantic_ms": semantic,
                    "semantic_filtered_ms": filtered, "hybrid_ms": hybrid, "keyword_and_ms": keyword,
                })
            );
            drop(c);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Real e5-small on 35 fictitious FR/EN messages: cross-lingual hits and
    /// cold query latency (model load with hash check, query embedding,
    /// search). E5_SMALL_DIR=... cargo test --release --features index
    /// real_e5_search -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_e5_search() {
        use std::time::Instant;
        let dir =
            std::env::var_os("E5_SMALL_DIR").map(Into::into).unwrap_or_else(|| model::default_dir(&MODELS[0]).unwrap());
        const MAIL: &[(&str, &str)] = &[
            ("Password reset", "To reset your Helios password, open the account page and click on forgot password."),
            ("Portail Helios", "Le portail Helios sera indisponible samedi matin pour maintenance."),
            ("Facture impayée", "La facture du mois dernier n'est toujours pas réglée, merci de procéder au paiement."),
            ("Panne du serveur", "Le serveur de production est tombé en panne cette nuit vers trois heures."),
            ("Summer vacation request", "Paul here: I would like to take two weeks of holidays in August."),
            ("Planning support", "Paul a envoyé le planning de l'équipe support pour la semaine."),
            ("Note de frais", "Voici ma note de frais pour le déplacement à Lyon : train et hôtel."),
            ("Lyon office", "The Lyon office moves to a new building next spring."),
            ("Contract renewal", "The Acme supplier contract expires in June; we need to renegotiate the terms."),
            ("Livraison Acme", "Acme a livré la commande de câbles ce matin."),
            ("Déjeuner d'équipe", "On se retrouve vendredi midi au restaurant italien pour le déjeuner d'équipe."),
            ("Birthday cake", "We are celebrating Marie's birthday with a cake in the kitchen at 4 pm."),
            ("Parking", "Le parking souterrain sera fermé lundi pour travaux de peinture."),
            ("Quarterly budget", "Please review the budget forecast for the third quarter before Friday."),
            ("Recrutement", "Nous recrutons un développeur Rust pour l'équipe plateforme."),
            ("Printer jammed", "The printer on the second floor is jammed again, paper stuck inside."),
            ("Formation sécurité", "La formation obligatoire à la sécurité incendie aura lieu jeudi."),
            ("Client complaint", "A customer complained about late delivery of the order and wants a refund."),
            ("Mise à jour logicielle", "Merci de redémarrer vos ordinateurs après la mise à jour de Windows."),
            ("Board meeting", "The Norvane board meeting is moved to next Tuesday at 10 in room B."),
            ("Visite Norvane", "Une délégation de Norvane visite l'usine mercredi."),
            ("Télétravail", "Le télétravail est autorisé deux jours par semaine à partir de septembre."),
            ("Travel booking", "Your flight to Montreal is confirmed, departure on the 12th."),
            ("Inventaire", "L'inventaire annuel du stock se fera le dernier week-end de décembre."),
            ("Newsletter", "This month's newsletter covers the new office opening and sustainability goals."),
            ("Rapport annuel", "Le rapport annuel doit être relu par la direction avant publication."),
            ("Phishing warning", "Do not click links in emails asking for your bank details; report them to IT."),
            ("Commande de fournitures", "Nous devons commander des stylos, des cahiers et du papier."),
            ("Wifi password", "The guest wifi network has a new access code this week."),
            ("Pot de départ", "Venez fêter le départ à la retraite de Jean jeudi soir."),
            ("Data backup", "Backups of the file server run every night at midnight."),
            ("Congé maternité", "Sophie sera en congé maternité à partir du mois prochain."),
            ("Website down", "The public website returns errors since this morning."),
            ("Réunion projet", "La réunion de lancement du projet aura lieu mardi dans la grande salle."),
            ("Heating issue", "The heating does not work on the third floor, it is very cold."),
        ];
        const COLD: &str = "comment réinitialiser mon mot de passe Helios";
        let mut f = Fake { folders: vec![folder(0x22, "Inbox")], ..Fake::default() };
        for (i, (s, b)) in MAIL.iter().enumerate() {
            f.add(0x1004 + i as u32 * 0x20, 0x22, Some(&format!("<r{i}@fict.invalid>")), s, b);
        }
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        sync(&mut c, &mut f, &Options::default()).unwrap();
        let mut e = crate::index::embed::Embedder::load(&MODELS[0], &dir).unwrap();
        vectors::run(&mut c, &mut [&mut e], &vectors::Options { batch: 8, max_time: None, reembed: false }).unwrap();
        drop(e);

        // Cold: load (hash check included) + query embedding + hybrid search.
        let t = Instant::now();
        let e = crate::index::embed::Embedder::load(&MODELS[0], &dir).unwrap();
        let load = t.elapsed();
        let v = e.embed_queries(&[COLD]).unwrap().remove(0);
        let embed = t.elapsed() - load;
        let x = Query { mode: Mode::Hybrid, vector: Some(v), model: Some(&MODELS[0]), ..q("x") };
        let hits = search(&c, &Query { text: COLD.into(), ..x }).unwrap();
        let cold = t.elapsed();
        let cold_rank = hits.iter().position(|h| h["subject"] == "Password reset").map(|p| p + 1);

        // (query, expected subject): cross-lingual both ways. The semantic
        // leg holds every message here, so a target absent from the any-word
        // keyword leg loses to any mail matching "de" or "the": some pairs
        // share a name, as real mail does, and a same-language distractor
        // with that name keeps the keyword leg alone from deciding.
        let cases = [
            (COLD, "Password reset"),
            ("unpaid invoice payment", "Facture impayée"),
            ("server outage last night", "Panne du serveur"),
            ("demande de congés de Paul en août", "Summer vacation request"),
            ("expense report for the Lyon business trip", "Note de frais"),
            ("renouvellement du contrat fournisseur Acme", "Contract renewal"),
            ("team lunch on friday", "Déjeuner d'équipe"),
            ("remote work policy", "Télétravail"),
            ("réunion du conseil d'administration Norvane", "Board meeting"),
            ("annual report review", "Rapport annuel"),
        ];
        let mut ranks = Vec::new();
        let mut warm = Vec::new();
        for (text, want) in cases {
            let t = Instant::now();
            let v = e.embed_queries(&[text]).unwrap().remove(0);
            let mut r = Vec::new();
            for mode in [Mode::Semantic, Mode::Hybrid] {
                let x = Query { mode, vector: Some(v.clone()), model: Some(&MODELS[0]), ..q(text) };
                let hits = search(&c, &x).unwrap();
                r.push(hits.iter().position(|h| h["subject"] == want).map(|p| p + 1));
            }
            warm.push(t.elapsed().as_secs_f64() * 1000.0);
            ranks.push(r);
        }
        let ms = |d: std::time::Duration| d.as_millis() as u64;
        eprintln!(
            "{}",
            json!({
                "messages": MAIL.len(), "cold_ms": ms(cold), "load_ms": ms(load), "query_embed_ms": ms(embed),
                "cold_search_ms": ms(cold - load - embed),
                "warm_embed_and_two_searches_ms_avg": (warm.iter().sum::<f64>() / warm.len() as f64).round(),
                "cold_hybrid_rank": cold_rank, "ranks_semantic_hybrid": ranks, "threads": std::env::var("RAYON_NUM_THREADS").ok(),
            })
        );
        for (r, (text, _)) in ranks.iter().zip(cases) {
            assert!(r[0].is_some_and(|p| p <= 3), "{text}: semantic {r:?}");
            assert!(r[1].is_some_and(|p| p <= 3), "{text}: hybrid {r:?}");
        }
    }
}
