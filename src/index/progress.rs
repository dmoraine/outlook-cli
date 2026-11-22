//! Progress of long runs (`sync`, `embed`): events on stderr (`bar` or
//! `jsonl`) and the live record in meta `sync_progress`, read by `status`.
//! Counts and durations only: never names, subjects or text.

use std::collections::VecDeque;
use std::io::Write;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value as Json, json};

use super::sync::{get_meta, set_meta};
use crate::error::{Error, Result};
use crate::timeutil::{format_ft, now_ft};

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
/// Meta key of the live record.
const META: &str = "sync_progress";
/// The rate covers this much of the current phase.
const WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// bar on a terminal, else jsonl.
    Auto,
    /// One self-overwriting line per phase.
    Bar,
    /// One {"progress":{...}} object per line.
    Jsonl,
    None,
}

impl Mode {
    /// `auto` as `bar` on a terminal, else `jsonl`.
    pub fn resolve(self, terminal: bool) -> Mode {
        match self {
            Mode::Auto if terminal => Mode::Bar,
            Mode::Auto => Mode::Jsonl,
            m => m,
        }
    }
}

/// Reporter of one run: phases (`scan`, `index`, `attachments`,
/// `embed_bodies`, `embed_attachments`, then `done`), each with done/total
/// counts. Reports at phase changes and at most once per interval; with
/// `meta`, also stores the record through the connection given, inside the
/// caller's batch transaction when there is one.
pub struct Progress {
    mode: Mode,
    out: Box<dyn Write>,
    interval: Duration,
    meta: bool,
    pub workers: Option<usize>,
    pub model: Option<&'static str>,
    start: Instant,
    started_at: Option<String>,
    phase: Option<&'static str>,
    done: u64,
    total: u64,
    /// (time, done) of the current phase, from the last one at or before the window start.
    samples: VecDeque<(Instant, u64)>,
    last: Option<Instant>,
    /// Length of the bar line on screen.
    width: usize,
}

impl Progress {
    pub fn new(mode: Mode, interval: Duration, out: Box<dyn Write>, meta: bool) -> Progress {
        Progress {
            mode,
            out,
            interval,
            meta,
            workers: None,
            model: None,
            start: Instant::now(),
            started_at: format_ft(now_ft(), false),
            phase: None,
            done: 0,
            total: 0,
            samples: VecDeque::new(),
            last: None,
            width: 0,
        }
    }

    /// Silent, nothing stored.
    pub fn none() -> Progress {
        Progress::new(Mode::None, DEFAULT_INTERVAL, Box::new(std::io::sink()), false)
    }

    /// Start phase `name` of `total` items; always reported.
    pub fn phase(&mut self, c: Option<&Connection>, name: &'static str, total: u64) -> Result<()> {
        self.phase_at(c, name, total, Instant::now())
    }

    /// `done` items of the current phase; reported once the interval has passed.
    pub fn update(&mut self, c: Option<&Connection>, done: u64) -> Result<()> {
        self.update_at(c, done, Instant::now())
    }

    /// The final `done` record (counts of the last phase); ends the bar line.
    pub fn finish(&mut self, c: Option<&Connection>) -> Result<()> {
        self.finish_at(c, Instant::now())
    }

    fn phase_at(&mut self, c: Option<&Connection>, name: &'static str, total: u64, now: Instant) -> Result<()> {
        self.end_line(now);
        (self.phase, self.total, self.done) = (Some(name), total, 0);
        self.samples = VecDeque::from([(now, 0)]);
        self.emit(c, now)
    }

    fn update_at(&mut self, c: Option<&Connection>, done: u64, now: Instant) -> Result<()> {
        self.done = done;
        self.samples.push_back((now, done));
        while self.samples.len() > 1 && self.samples[1].0 + WINDOW <= now {
            self.samples.pop_front();
        }
        match self.last {
            Some(l) if now.saturating_duration_since(l) < self.interval => Ok(()),
            _ => self.emit(c, now),
        }
    }

    fn finish_at(&mut self, c: Option<&Connection>, now: Instant) -> Result<()> {
        self.end_line(now);
        self.phase = Some("done");
        self.samples.clear();
        self.emit(c, now)
    }

    fn record(&self, now: Instant) -> Json {
        let rate = self.samples.front().and_then(|&(t, d)| {
            let dt = now.saturating_duration_since(t).as_secs_f64();
            (dt > 0.0).then(|| self.done.saturating_sub(d) as f64 / dt)
        });
        let eta = rate.filter(|r| *r > 0.0).map(|r| (self.total.saturating_sub(self.done) as f64 / r).ceil() as u64);
        json!({
            "phase": self.phase.unwrap_or("done"),
            "done": self.done,
            "total": self.total,
            // Rounded down to 0.1: 100 only when done.
            "percent": (self.total > 0).then(|| (self.done.min(self.total) * 1000 / self.total) as f64 / 10.0),
            "rate": rate.map(|r| (r * 100.0).round() / 100.0),
            "eta_s": eta,
            "elapsed_s": now.saturating_duration_since(self.start).as_secs(),
            "workers": self.workers,
            "model": self.model,
        })
    }

    /// Whole lines in one write: JSONL stays parseable among other stderr
    /// lines. A failing stderr never fails the run.
    fn write(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes()).and_then(|_| self.out.flush());
    }

    /// Bar mode: the current phase's line with its latest counts, then a newline.
    fn end_line(&mut self, now: Instant) {
        if self.mode == Mode::Bar && self.phase.is_some_and(|p| p != "done") {
            let line = self.bar(now);
            self.write(&format!("{line}\n"));
            self.width = 0;
        }
    }

    /// "\r" + the bar line, padded over the previous one.
    fn bar(&mut self, now: Instant) -> String {
        let line = bar(&self.record(now));
        let pad = self.width.saturating_sub(line.len());
        self.width = line.len();
        format!("\r{line}{}", " ".repeat(pad))
    }

    fn emit(&mut self, c: Option<&Connection>, now: Instant) -> Result<()> {
        self.last = Some(now);
        let r = self.record(now);
        match self.mode {
            Mode::Jsonl => self.write(&format!("{}\n", json!({ "progress": r }))),
            Mode::Bar if r["phase"] != "done" => {
                let line = self.bar(now);
                self.write(&line);
            }
            _ => {}
        }
        if let (true, Some(c)) = (self.meta, c) {
            let mut r = r;
            let o = r.as_object_mut().unwrap();
            o.insert("pid".into(), json!(std::process::id()));
            o.insert("started_at".into(), json!(self.started_at));
            o.insert("updated_at".into(), json!(format_ft(now_ft(), false)));
            o.insert("interval_s".into(), json!(self.interval.as_secs()));
            set_meta(c, META, &r.to_string())?;
        }
        Ok(())
    }
}

/// A run ending in an error: its error line starts on a new line.
impl Drop for Progress {
    fn drop(&mut self) {
        self.end_line(Instant::now());
    }
}

/// "embed_bodies       41 % 12,345/30,112  3.9/s  ETA 1h15m". ASCII only.
fn bar(r: &Json) -> String {
    let n = |k: &str| r[k].as_u64().unwrap_or(0);
    let pct = r["percent"].as_f64().map_or("--".into(), |p| format!("{p:.0}"));
    let rate = r["rate"].as_f64().map_or("-".into(), |x| format!("{x:.1}"));
    let eta = r["eta_s"].as_u64().map_or("-".into(), duration);
    format!(
        "{:<17} {pct:>3} % {}/{}  {rate}/s  ETA {eta}",
        r["phase"].as_str().unwrap_or(""),
        thousands(n("done")),
        thousands(n("total"))
    )
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut o = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            o.push(',');
        }
        o.push(ch);
    }
    o
}

/// 1h15m, 5m03s, 42s.
fn duration(s: u64) -> String {
    match s {
        3600.. => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        60.. => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{s}s"),
    }
}

/// The stored record as `status` prints it, `done` phase included; None
/// without one. `held`: a process holds the index's sync lock. Stale: the
/// lock is free and the record is older than twice its writer's interval
/// (a crashed or killed run).
pub fn live(c: &Connection, held: bool) -> Result<Option<Json>> {
    let Some(s) = get_meta(c, META)? else { return Ok(None) };
    let r: Json = serde_json::from_str(&s).map_err(|e| Error::Format(format!("stored progress: {e}")))?;
    let interval = r["interval_s"].as_i64().unwrap_or(DEFAULT_INTERVAL.as_secs() as i64);
    let age = r["updated_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (chrono::Utc::now() - t.to_utc()).num_seconds());
    let mut o = serde_json::Map::new();
    for k in ["phase", "done", "total", "percent", "rate", "eta_s", "elapsed_s", "pid", "updated_at"] {
        o.insert(k.into(), r[k].clone());
    }
    o.insert("stale".into(), json!(!held && age.is_none_or(|a| a > 2 * interval)));
    Ok(Some(Json::Object(o)))
}

/// `status.running`: the live record, or null when there is none or the run finished.
pub fn running(c: &Connection, held: bool) -> Result<Json> {
    Ok(live(c, held)?.filter(|r| r["phase"] != "done").unwrap_or(Json::Null))
}

/// A sink shared with the test that reads it.
#[cfg(test)]
pub mod testing {
    use std::cell::RefCell;
    use std::rc::Rc;

    pub struct Buf(pub Rc<RefCell<Vec<u8>>>);

    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// (reader, sink).
    pub fn buf() -> (Rc<RefCell<Vec<u8>>>, Box<dyn std::io::Write>) {
        let b = Rc::new(RefCell::new(Vec::new()));
        (b.clone(), Box::new(Buf(b)))
    }

    pub fn text(b: &Rc<RefCell<Vec<u8>>>) -> String {
        String::from_utf8(b.borrow().clone()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::io::IsTerminal;

    use serde_json::json;

    use super::testing::*;
    use super::*;
    use crate::index::sync::fake::*;
    use crate::index::sync::{self, MailSource, Options, get_meta, set_meta};
    use crate::index::{migrate, search};
    use crate::timeutil::{format_ft, now_ft};

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn events(b: &std::rc::Rc<std::cell::RefCell<Vec<u8>>>) -> Vec<Json> {
        text(b).lines().map(|l| serde_json::from_str::<Json>(l).unwrap()["progress"].clone()).collect()
    }

    #[test]
    fn auto_is_jsonl_on_a_pipe() {
        let (_r, w) = std::io::pipe().unwrap();
        #[cfg(unix)]
        let w = std::os::fd::OwnedFd::from(w);
        #[cfg(windows)]
        let w = std::os::windows::io::OwnedHandle::from(w);
        assert!(!w.is_terminal());
        assert_eq!(Mode::Auto.resolve(w.is_terminal()), Mode::Jsonl);
        assert_eq!(Mode::Auto.resolve(true), Mode::Bar);
        for m in [Mode::Bar, Mode::Jsonl, Mode::None] {
            assert_eq!((m.resolve(true), m.resolve(false)), (m, m));
        }
    }

    /// Phase changes always, updates at most once per interval, then `done`.
    #[test]
    fn throttled_by_interval() {
        let (b, out) = buf();
        let mut p = Progress::new(Mode::Jsonl, s(10), out, false);
        let t = Instant::now();
        p.phase_at(None, "index", 100, t).unwrap();
        p.update_at(None, 10, t + s(1)).unwrap();
        p.update_at(None, 20, t + s(9)).unwrap();
        p.update_at(None, 30, t + s(10)).unwrap();
        p.update_at(None, 40, t + s(15)).unwrap();
        p.phase_at(None, "attachments", 0, t + s(16)).unwrap();
        p.update_at(None, 0, t + s(17)).unwrap();
        p.finish_at(None, t + s(18)).unwrap();
        let e = events(&b);
        let got: Vec<(&str, u64)> =
            e.iter().map(|e| (e["phase"].as_str().unwrap(), e["done"].as_u64().unwrap())).collect();
        assert_eq!(got, [("index", 0), ("index", 30), ("attachments", 0), ("done", 0)], "{e:?}");
        let keys: Vec<&str> = e[1].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["phase", "done", "total", "percent", "rate", "eta_s", "elapsed_s", "workers", "model"]);
        assert_eq!(
            (e[1]["percent"].as_f64(), e[1]["rate"].as_f64(), e[1]["eta_s"].as_u64()),
            (Some(30.0), Some(3.0), Some(24))
        );
        assert_eq!(e[2]["percent"], Json::Null, "nothing to do: no percent");
        assert_eq!((e[3]["rate"].clone(), e[3]["eta_s"].clone()), (Json::Null, Json::Null));
        assert!(e[3]["elapsed_s"].as_u64().is_some());
    }

    /// The rate covers the last minute of the current phase.
    #[test]
    fn rate_over_the_last_minute() {
        let (b, out) = buf();
        let mut p = Progress::new(Mode::Jsonl, s(10), out, false);
        p.workers = Some(4);
        p.model = Some("e5-base");
        let t = Instant::now();
        p.phase_at(None, "embed_bodies", 1000, t).unwrap();
        // 1/s for a minute, then 3/s.
        for i in 1..=12u64 {
            let done = if i <= 6 { i * 10 } else { 60 + (i - 6) * 30 };
            p.update_at(None, done, t + s(i * 10)).unwrap();
        }
        let last = events(&b).pop().unwrap();
        assert_eq!((last["done"].as_u64(), last["rate"].as_f64()), (Some(240), Some(3.0)), "{last}");
        assert_eq!((last["eta_s"].as_u64(), last["elapsed_s"].as_u64()), (Some(254), Some(120)), "{last}");
        assert_eq!((last["workers"].as_u64(), last["model"].as_str()), (Some(4), Some("e5-base")));
    }

    #[test]
    fn bar_lines() {
        let (b, out) = buf();
        let mut p = Progress::new(Mode::Bar, s(10), out, false);
        let t = Instant::now();
        p.phase_at(None, "index", 10, t).unwrap();
        p.phase_at(None, "embed_bodies", 30_112, t + s(1)).unwrap();
        p.update_at(None, 12_345, t + s(3166)).unwrap();
        p.finish_at(None, t + s(3167)).unwrap();
        let out = text(&b);
        assert!(out.is_ascii() && out.starts_with('\r') && out.ends_with('\n'), "{out:?}");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3, "one line per phase, then the newline: {out:?}");
        let last = lines[1].rsplit('\r').next().unwrap();
        for want in ["embed_bodies", "41 %", "12,345/30,112", "3.9/s", "ETA 1h15m"] {
            assert!(last.contains(want), "{want:?} in {last:?}");
        }
        assert!(!out.contains('{') && !out.contains("done"), "{out:?}");
        // none: silent.
        let (b, out) = buf();
        let mut p = Progress::new(Mode::None, s(10), out, false);
        p.phase(None, "index", 1).unwrap();
        p.finish(None).unwrap();
        assert!(b.borrow().is_empty());
    }

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c
    }

    fn record(phase: &str, ago_s: i64) -> String {
        json!({
            "phase": phase, "done": 1, "total": 2, "percent": 50.0, "rate": 0.5, "eta_s": 2, "elapsed_s": 9,
            "workers": 1, "model": "e5-base", "pid": 4242, "started_at": "2026-11-22T10:00:00Z",
            "updated_at": format_ft(now_ft() - ago_s * 10_000_000, false), "interval_s": 10,
        })
        .to_string()
    }

    #[test]
    fn stale_record() {
        let c = db();
        assert_eq!(running(&c, false).unwrap(), Json::Null, "no record");
        set_meta(&c, "sync_progress", &record("index", 1)).unwrap();
        let r = running(&c, false).unwrap();
        let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["phase", "done", "total", "percent", "rate", "eta_s", "elapsed_s", "pid", "updated_at", "stale"]
        );
        assert_eq!(
            (r["phase"].as_str(), r["pid"].as_u64(), r["stale"].as_bool()),
            (Some("index"), Some(4242), Some(false))
        );
        // Older than 2 x interval with nobody holding the lock: stale.
        set_meta(&c, "sync_progress", &record("index", 21)).unwrap();
        assert_eq!(running(&c, false).unwrap()["stale"], true);
        assert_eq!(running(&c, true).unwrap()["stale"], false, "the lock is held: still running");
        set_meta(&c, "sync_progress", &record("done", 3600)).unwrap();
        assert_eq!(running(&c, false).unwrap(), Json::Null, "finished");
        assert_eq!(live(&c, false).unwrap().unwrap()["phase"], "done");
    }

    #[test]
    fn lock_held() {
        let dir = std::env::temp_dir().join(format!("outlook-index-held-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("index.sqlite");
        assert!(!sync::held(&p), "no lock file");
        let l = sync::lock(&p).unwrap();
        assert!(sync::held(&p));
        drop(l);
        assert!(!sync::held(&p));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Reads `status` from another connection each time a message is opened.
    struct Spy {
        inner: Fake,
        path: std::path::PathBuf,
        seen: Vec<Json>,
    }

    impl MailSource for Spy {
        fn revision(&mut self) -> Result<String> {
            self.inner.revision()
        }
        fn folders(&mut self) -> Result<Vec<crate::store::Folder>> {
            self.inner.folders()
        }
        fn entries(&mut self, f: crate::ndb::Nid) -> Result<Vec<sync::Entry>> {
            self.inner.entries(f)
        }
        fn open(&mut self, nid: crate::ndb::Nid) -> Result<sync::Opened> {
            let c = crate::index::open_read(&self.path).unwrap();
            let mut v = search::status(&c).unwrap();
            v["running"] = running(&c, true).unwrap();
            self.seen.push(v);
            self.inner.open(nid)
        }
        fn well_known(&mut self, f: &[crate::store::Folder]) -> Result<crate::store::WellKnown> {
            self.inner.well_known(f)
        }
        fn attachments(&mut self, nid: crate::ndb::Nid) -> Result<Vec<crate::store::Attachment>> {
            self.inner.attachments(nid)
        }
        fn attachment_data(&mut self, nid: crate::ndb::Nid, a: &crate::store::Attachment) -> Result<Vec<u8>> {
            self.inner.attachment_data(nid, a)
        }
    }

    #[test]
    fn status_during_a_run() {
        let dir = std::env::temp_dir().join(format!("outlook-index-live-{}", std::process::id()));
        let path = dir.join("index.sqlite");
        let mut c = crate::index::open(&path).unwrap();
        let mut spy = Spy { inner: mailbox(), path: path.clone(), seen: Vec::new() };
        let mut p = Progress::new(Mode::None, Duration::ZERO, Box::new(std::io::sink()), true);
        let o = Options { batch: 1, ..Options::default() };
        sync::sync_with_progress(&mut c, &mut spy, None, &o, &mut p).unwrap();
        let r: Vec<(Json, Json)> =
            spy.seen.iter().map(|v| (v["running"]["phase"].clone(), v["running"]["done"].clone())).collect();
        assert_eq!(r, [(json!("index"), json!(0)), (json!("index"), json!(1)), (json!("index"), json!(2))]);
        assert_eq!(spy.seen[2]["messages"], 2, "committed batches visible: {}", spy.seen[2]);
        assert_eq!(spy.seen[2]["running"]["total"], 3);
        p.finish(Some(&c)).unwrap();
        let rec: Json = serde_json::from_str(&get_meta(&c, "sync_progress").unwrap().unwrap()).unwrap();
        assert_eq!((rec["phase"].as_str(), rec["pid"].as_u64()), (Some("done"), Some(std::process::id() as u64)));
        assert!(rec["started_at"].as_str().is_some() && rec["updated_at"].as_str().is_some(), "{rec}");
        // A writer inside a transaction does not block status.
        let tx = c.transaction().unwrap();
        set_meta(&tx, "sync_progress", &record("embed_bodies", 0)).unwrap();
        let other = crate::index::open_read(&path).unwrap();
        assert_eq!(running(&other, true).unwrap(), Json::Null, "last committed record: done");
        search::status(&other).unwrap();
        tx.commit().unwrap();
        assert_eq!(running(&other, true).unwrap()["phase"], "embed_bodies");
        drop((c, other));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
