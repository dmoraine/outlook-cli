//! `outlook-index search` (keyword, FTS5) and `status`.

use std::collections::HashSet;

use rusqlite::{Connection, params};
use serde_json::{Value as Json, json};

use super::db_err;
use super::sync::{get_meta, totals};
use crate::error::{Error, Result};
use crate::store::MSGFLAG_READ;
use crate::timeutil::format_ft;

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
}

/// FTS5 expression for free text: every word becomes a quoted phrase, so
/// operators, column filters and quotes in the input are plain text.
/// None when no word has a letter or digit.
pub fn fts_query(q: &str) -> Option<String> {
    // NUL ends an FTS5 string: control characters become spaces.
    let q = q.replace(char::is_control, " ");
    let words: Vec<String> = q
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// Default and maximum excerpt length (characters).
pub const EXCERPT_CHARS: usize = 300;
pub const EXCERPT_MAX: usize = 2000;

const SQL: &str = "
    SELECT m.id, m.message_id, m.received, m.subject, m.from_name, m.from_email, m.display_to, m.display_cc,
           snippet(chunks_fts, 1, '', '', '...', ?6), bm25(chunks_fts),
           (SELECT l.nid FROM locations l WHERE l.message = m.id
             ORDER BY (l.folder IN (SELECT value FROM json_each(?4))) DESC, l.nid LIMIT 1) AS nid,
           m.sent, m.size, m.class, m.has_attachments, m.sender
    FROM chunks_fts
    JOIN chunks c ON c.id = chunks_fts.rowid
    JOIN messages m ON m.id = c.message
    WHERE chunks_fts MATCH ?1
      AND (?2 IS NULL OR m.received >= ?2)
      AND (?3 IS NULL OR m.received < ?3)
      AND (?4 = '[]' OR EXISTS (SELECT 1 FROM locations l WHERE l.message = m.id
                                AND l.folder IN (SELECT value FROM json_each(?4))))
      AND (?5 = 0 OR m.has_attachments = 1)
    ORDER BY bm25(chunks_fts)";

/// Folder ids for -F specs, case-insensitive: a name, or a path with its
/// subfolders ("Inbox" matches "Inbox/Projects", not "Inboxes").
fn folder_ids(c: &Connection, specs: &[String]) -> Result<Vec<i64>> {
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

/// Current folder path and flags of a location.
fn location(c: &Connection, nid: i64) -> Result<(Option<String>, i64)> {
    c.query_row(
        "SELECT f.path, l.flags FROM locations l LEFT JOIN folders f ON f.nid = l.folder WHERE l.nid = ?1",
        [nid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(db_err)
}

/// Best matching chunk per message, best first.
// ponytail: walks every matching chunk to group; fine for one mailbox, add a
// per-message MIN(bm25) pass if very common words get slow.
pub fn search(c: &Connection, q: &Query) -> Result<Vec<Json>> {
    let expr = fts_query(&q.text).ok_or_else(|| Error::Usage("the query has no word to search for".into()))?;
    let excerpt_max = if q.excerpt_max == 0 { EXCERPT_CHARS } else { q.excerpt_max.min(EXCERPT_MAX) };
    // snippet() counts tokens (FTS5 caps at 64); the char bound is applied after.
    let tokens = (excerpt_max / 6).clamp(4, 64) as i64;
    let folders = serde_json::to_string(&folder_ids(c, &q.folders)?).unwrap();
    let low = |v: &[String]| v.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
    let (from, to) = (low(&q.from), low(&q.to));
    let any = |want: &[String], hay: &[&Option<String>]| {
        let hay = hay.iter().filter_map(|h| h.as_deref()).collect::<Vec<_>>().join(" ").to_lowercase();
        want.is_empty() || want.iter().any(|w| hay.contains(w.as_str()))
    };
    let mut st = c.prepare(SQL).map_err(db_err)?;
    let mut rows = st.query(params![expr, q.since, q.until, folders, q.has_attachments, tokens]).map_err(db_err)?;
    let (mut seen, mut out) = (HashSet::new(), Vec::new());
    while out.len() < q.limit.max(1) {
        let Some(r) = rows.next().map_err(db_err)? else { break };
        let id: i64 = r.get(0).map_err(db_err)?;
        let s = |i| r.get::<_, Option<String>>(i).map_err(db_err);
        let (name, email, sender, to_d, cc_d) = (s(4)?, s(5)?, s(15)?, s(6)?, s(7)?);
        if seen.contains(&id) || !any(&from, &[&name, &email, &sender]) || !any(&to, &[&to_d, &cc_d]) {
            continue;
        }
        seen.insert(id);
        let nid: i64 = r.get(10).map_err(db_err)?;
        let (folder, flags) = location(c, nid)?;
        let score: f64 = r.get(9).map_err(db_err)?;
        let time = |i| r.get::<_, Option<i64>>(i).map(|t| t.and_then(|t| format_ft(t, q.local_time))).map_err(db_err);
        out.push(json!({
            "id": nid,
            "message_id": s(1)?,
            "folder": folder,
            "received": time(2)?,
            "sent": time(11)?,
            "subject": s(3)?,
            "from": { "name": name.or(sender), "email": email },
            "to": to_d,
            "cc": cc_d,
            "size": r.get::<_, Option<i64>>(12).map_err(db_err)?,
            "class": s(13)?,
            "has_attachments": r.get::<_, bool>(14).map_err(db_err)?,
            "read": flags & MSGFLAG_READ != 0,
            "excerpt": crate::body::truncate(s(8)?.unwrap_or_default(), excerpt_max).0,
            "source": "body",
            "ranks": { "keyword": out.len() + 1 },
            "scores": { "keyword": (-score * 1e4).round() / 1e4 },
        }));
    }
    Ok(out)
}

/// Aggregate counts only.
pub fn status(c: &Connection) -> Result<Json> {
    let at = get_meta(c, "sync_at")?.and_then(|t| t.parse().ok()).and_then(|t| format_ft(t, false));
    let last = get_meta(c, "sync_last")?.and_then(|s| serde_json::from_str::<Json>(&s).ok());
    let mut v = totals(c)?;
    let o = v.as_object_mut().unwrap();
    o.insert("schema".into(), json!(super::schema_version(c)?));
    o.insert("embedded".into(), json!(0));
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
        assert_eq!(s["embedded"], 0);
        assert_eq!(s["last_sync"]["complete"], true);
        assert_eq!(s["schema"], crate::index::schema_version(&c).unwrap());
        assert!(s["last_sync"]["revision"].is_string() && s["last_sync"]["at"].is_string(), "{s}");
        assert_eq!(s["last_sync"]["summary"]["new"], 5, "{s}");
        let t = s.to_string();
        for m in ["Zed", "Ann", "fict", "Budget", "budget", "Inbox", "Archive", "Lunch", "kickoff"] {
            assert!(!t.contains(m), "status leaks {m:?}: {t}");
        }
    }
}
