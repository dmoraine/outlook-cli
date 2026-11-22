//! `outlook-index doctor`: aggregate-only diagnostics of the mailbox and the
//! machine. The output never contains mail content or names: only counts,
//! sizes, durations, classes, depths, file extensions and error kinds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use crate::body::text_of;
use crate::error::{Error, Result, with_retry};
use crate::ndb::Nid;
use crate::store::{Attachment, Folder, MSGFLAG_HASATTACH, MSGFLAG_READ, Store, is_mail_folder};
use crate::timeutil::format_ft;

/// One contents-table row, reduced to what doctor needs.
pub struct Row {
    pub nid: Nid,
    pub class: Option<String>,
    pub received: i64,
    pub flags: i64,
    /// Non-empty Internet Message-ID (sync identity; else a fingerprint).
    pub has_message_id: bool,
}

/// A sampled message.
pub struct Sample {
    pub text: Option<String>,
    pub text_time: Duration,
    pub attachments: Vec<Attachment>,
}

/// What doctor reads. Implemented over a `Store`, and by a fake in tests.
pub trait Source {
    fn folders(&mut self) -> Result<Vec<Folder>>;
    fn rows(&mut self, folder: Nid) -> Result<Vec<Row>>;
    fn sample(&mut self, nid: Nid) -> Result<Sample>;
}

/// Chunking assumed for the chunk estimate (characters).
pub const CHUNK_CHARS: usize = 1750;
pub const CHUNK_OVERLAP: usize = 200;

fn chunks(chars: usize) -> usize {
    if chars <= CHUNK_CHARS {
        usize::from(chars > 0)
    } else {
        1 + (chars - CHUNK_CHARS).div_ceil(CHUNK_CHARS - CHUNK_OVERLAP)
    }
}

/// Extensions reported by name; anything else is "other", so that an odd
/// file name never leaks through its "extension".
const EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "docm", "xls", "xlsx", "xlsm", "ppt", "pptx", "txt", "csv", "htm", "html", "md", "rtf",
    "odt", "ods", "odp", "xml", "json", "eml", "msg", "ics", "vcf", "zip", "7z", "rar", "gz", "png", "jpg", "jpeg",
    "gif", "bmp", "tif", "tiff", "svg", "heic", "webp", "mp3", "mp4", "m4a", "wav", "mov", "p7s", "p7m", "dat", "bin",
    "exe", "log", "one", "vsdx",
];

fn extension(name: Option<&str>) -> &'static str {
    let Some(e) = name.and_then(|n| Path::new(n).extension()).and_then(|e| e.to_str()) else {
        return "none";
    };
    let e = e.to_ascii_lowercase();
    EXTENSIONS.iter().find(|x| **x == e).copied().unwrap_or("other")
}

/// min / p50 / p95 / max / sum (nearest-rank percentiles), or null when empty.
fn dist(mut v: Vec<u64>) -> Json {
    if v.is_empty() {
        return Json::Null;
    }
    v.sort_unstable();
    let p = |q: usize| v[(v.len() * q).div_ceil(100).max(1) - 1];
    json!({ "min": v[0], "p50": p(50), "p95": p(95), "max": v[v.len() - 1], "sum": v.iter().sum::<u64>() })
}

/// Class segments reported by name. A class is reduced to its first two
/// segments and any other segment becomes "other": custom form names
/// ("IPM.Note.Contoso_Expense") never get through.
const CLASS_PARTS: &[&str] = &[
    "IPM",
    "IPF",
    "REPORT",
    "Note",
    "Appointment",
    "Schedule",
    "Contact",
    "DistList",
    "Task",
    "TaskRequest",
    "StickyNote",
    "Activity",
    "Journal",
    "Post",
    "Document",
    "Sharing",
    "Configuration",
    "Outlook",
    "Recall",
    "Imap",
    "Conversation",
    "Microsoft",
    "OLE",
    "Files",
    "SkypeTeams",
];

fn class_prefix(c: Option<&str>) -> String {
    let Some(c) = c else { return "none".into() };
    let known = |p: &str| CLASS_PARTS.iter().find(|k| k.eq_ignore_ascii_case(p)).copied().unwrap_or("other");
    c.split('.').take(2).map(known).collect::<Vec<_>>().join(".")
}

/// `n` distinct indices out of `0..len`, uniformly at random (partial
/// Fisher-Yates over splitmix64).
// ponytail: modulo bias is < len / 2^64, irrelevant for a diagnostic sample.
fn pick(len: usize, n: usize, mut seed: u64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..len).collect();
    for i in 0..n.min(len) {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        idx.swap(i, i + (z % (len - i) as u64) as usize);
    }
    idx.truncate(n);
    idx
}

/// Total physical memory in bytes, None when unknown.
#[cfg(target_os = "linux")]
fn total_ram() -> Option<u64> {
    meminfo_total(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(windows)]
fn total_ram() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        load: u32,
        total_phys: u64,
        rest: [u64; 6],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(m: *mut MemoryStatusEx) -> i32;
    }
    let mut m = MemoryStatusEx { length: size_of::<MemoryStatusEx>() as u32, load: 0, total_phys: 0, rest: [0; 6] };
    // SAFETY: `m` is a MEMORYSTATUSEX (64 bytes) with dwLength set, as the API requires.
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.total_phys)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn total_ram() -> Option<u64> {
    None
}

/// "MemTotal:  16314312 kB" line of /proc/meminfo, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn meminfo_total(s: &str) -> Option<u64> {
    let v = s.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim();
    v.strip_suffix("kB")?.trim().parse::<u64>().ok()?.checked_mul(1024)
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn bump<K: Ord>(m: &mut BTreeMap<K, u64>, k: K) {
    *m.entry(k).or_default() += 1;
}

/// Aggregate report over the mail folders of `src`, sampling `sample`
/// messages chosen at random from `seed`.
pub fn report(src: &mut dyn Source, sample: usize, seed: u64) -> Json {
    let mut errors: BTreeMap<&'static str, u64> = BTreeMap::new();

    let t = Instant::now();
    let folders = src.folders().unwrap_or_else(|e| {
        bump(&mut errors, e.kind());
        Vec::new()
    });
    let folders_time = t.elapsed();
    let (mut f_class, mut f_depth) = (BTreeMap::new(), BTreeMap::new());
    for f in &folders {
        bump(&mut f_class, class_prefix(f.class.as_deref()));
        bump(&mut f_depth, f.depth);
    }
    let mail: Vec<&Folder> = folders.iter().filter(|f| is_mail_folder(f)).collect();

    let t = Instant::now();
    let mut rows = Vec::new();
    for f in &mail {
        match src.rows(f.nid) {
            Ok(r) => rows.extend(r),
            Err(e) => bump(&mut errors, e.kind()),
        }
    }
    let scan_time = t.elapsed();
    let (mut m_class, mut m_year) = (BTreeMap::new(), BTreeMap::new());
    let (mut unread, mut with_att, mut with_mid) = (0, 0, 0);
    for r in &rows {
        with_mid += u64::from(r.has_message_id);
        bump(&mut m_class, class_prefix(r.class.as_deref()));
        bump(&mut m_year, format_ft(r.received, false).map_or("unknown".into(), |s| s[..4].to_string()));
        unread += u64::from(r.flags & MSGFLAG_READ == 0);
        with_att += u64::from(r.flags & MSGFLAG_HASATTACH != 0);
    }

    let (mut read_us, mut text_us, mut open_text_us, mut text_chars) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut read, mut no_text, mut body_chunks) = (0u64, 0u64, 0usize);
    let (mut a_ext, mut a_method, mut a_size) = (BTreeMap::new(), BTreeMap::new(), Vec::new());
    let (mut a_count, mut a_inline, mut a_big) = (0u64, 0u64, 0u64);
    let t = Instant::now();
    for i in pick(rows.len(), sample, seed) {
        let s = Instant::now();
        match src.sample(rows[i].nid) {
            Ok(m) => {
                read += 1;
                let total = s.elapsed();
                open_text_us.push(total.as_micros() as u64);
                read_us.push(total.saturating_sub(m.text_time).as_micros() as u64);
                text_us.push(m.text_time.as_micros() as u64);
                match m.text {
                    Some(text) => {
                        let c = text.chars().count();
                        text_chars.push(c as u64);
                        body_chunks += chunks(c);
                    }
                    None => no_text += 1,
                }
                for a in &m.attachments {
                    a_count += 1;
                    a_inline += u64::from(a.inline);
                    bump(&mut a_ext, extension(a.name.as_deref()));
                    bump(&mut a_method, a.method);
                    let size = a.size.unwrap_or(0).max(0) as u64;
                    a_big += u64::from(size > 20 << 20);
                    a_size.push(size);
                }
            }
            Err(e) => bump(&mut errors, e.kind()),
        }
    }
    let sample_time = t.elapsed();
    let per_message = if read > 0 { body_chunks as f64 / read as f64 } else { 0.0 };

    json!({
        "machine": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cpus": std::thread::available_parallelism().map_or(0, |n| n.get()),
            "ram_bytes": total_ram(),
        },
        "folders": {
            "total": folders.len(),
            "mail": mail.len(),
            "by_class": f_class,
            "by_depth": f_depth,
        },
        "messages": {
            "total": rows.len(),
            "unread": unread,
            "with_attachments": with_att,
            "with_message_id": with_mid,
            "without_message_id": rows.len() as u64 - with_mid,
            "by_class": m_class,
            "by_year": m_year,
        },
        "timings_ms": {
            "folders": ms(folders_time),
            "full_scan": ms(scan_time),
            "sample": ms(sample_time),
        },
        "full_scan_rows_per_s": round2(rows.len() as f64 / scan_time.as_secs_f64().max(1e-6)),
        "sample": {
            "requested": sample,
            "read": read,
            "no_text": no_text,
            "open_text_us": dist(open_text_us),
            "read_us": dist(read_us),
            "text_us": dist(text_us),
            "text_chars": dist(text_chars),
            "body_chunks": {
                "chunk_chars": CHUNK_CHARS,
                "overlap": CHUNK_OVERLAP,
                "sample_total": body_chunks,
                "per_message": round2(per_message),
                "mailbox_estimate": (per_message * rows.len() as f64).round() as u64,
            },
        },
        "attachments": {
            "count": a_count,
            "inline": a_inline,
            "over_20mb": a_big,
            "by_extension": a_ext,
            "by_method": a_method,
            "bytes": dist(a_size),
        },
        "errors": errors,
    })
}

/// `Source` over the real file. Every read retries on inconsistent data,
/// reopening the file (Outlook may be writing it).
pub struct StoreSource {
    path: PathBuf,
    store: Store,
}

impl StoreSource {
    pub fn open(path: &Path) -> Result<StoreSource> {
        Ok(StoreSource { path: path.to_path_buf(), store: with_retry(|| Store::open(path))? })
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// Run `f`, reopening the file and retrying on inconsistent data.
    pub(crate) fn retry<T>(&mut self, mut f: impl FnMut(&Store) -> Result<T>) -> Result<T> {
        let mut first = true;
        with_retry(|| {
            if !std::mem::take(&mut first) {
                self.store = Store::open(&self.path)?;
            }
            f(&self.store)
        })
    }
}

impl Source for StoreSource {
    fn folders(&mut self) -> Result<Vec<Folder>> {
        self.retry(|s| s.folders(false))
    }

    fn rows(&mut self, folder: Nid) -> Result<Vec<Row>> {
        self.retry(|s| {
            let mut v = Vec::new();
            s.scan_folder(folder, &mut |r| {
                v.push(Row {
                    nid: r.nid,
                    class: r.class.get(),
                    received: r.received,
                    flags: r.flags,
                    has_message_id: r.message_id.get().is_some_and(|m| !m.trim().is_empty()),
                });
                true
            })?;
            Ok(v)
        })
    }

    fn sample(&mut self, nid: Nid) -> Result<Sample> {
        self.retry(|s| {
            let m = s.message(nid)?;
            let t = Instant::now();
            let text = text_of(&m);
            let text_time = t.elapsed();
            Ok(Sample { text, text_time, attachments: m.attachments()? })
        })
    }
}

/// Keep the kind, replace the message with a fixed one: messages may hold
/// the file path (user name), folder names or, from a parser panic, mail text.
pub fn sanitize(e: Error) -> Error {
    match e {
        Error::Io(_) => Error::Io("cannot read the file or write the output".into()),
        Error::Format(_) => Error::Format("not a supported OST/PST file".into()),
        Error::Corrupt(_) => Error::Corrupt("the file changed during the read; retry".into()),
        Error::NotFound(_) => Error::NotFound("a required item is missing".into()),
        Error::Usage(_) => Error::Usage("no OST file or data directory found; pass --file / --index".into()),
    }
}

/// Full doctor report: tool, SQLite, index location, file and mailbox stats.
pub fn run(file: &Path, index: &Path, sample: usize) -> Result<Json> {
    let t = Instant::now();
    let mut src = StoreSource::open(file).map_err(sanitize)?;
    let open_time = t.elapsed();
    let ndb = &src.store.ndb;
    let file_json = json!({
        "kind": if ndb.is_ost { "ost" } else { "pst" },
        "format": ndb.format.name(),
        "bytes": ndb.file_len(),
        "open_ms": ms(open_time),
    });
    let seed = std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), open_time);
    let mut v = report(&mut src, sample, seed);
    let o = v.as_object_mut().unwrap();
    o.insert("tool".into(), json!({ "name": "outlook-index", "version": env!("CARGO_PKG_VERSION") }));
    o.insert("sqlite".into(), json!({ "version": rusqlite::version(), "fts5": super::fts5_available() }));
    let size = std::fs::metadata(index).ok().map(|m| m.len());
    o.insert(
        "index".into(),
        json!({ "exists": size.is_some(), "bytes": size, "under_onedrive": super::under_onedrive(index) }),
    );
    o.insert("file".into(), file_json);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    const MARKERS: &[&str] = &["SECRET", "Alice", "bob@example.org", "Quarterly"];

    struct Fake;

    fn folder(nid: Nid, name: &str, class: Option<&str>, depth: usize) -> Folder {
        Folder {
            nid,
            parent: None,
            name: name.into(),
            path: format!("SECRET_ROOT/{name}"),
            class: class.map(str::to_string),
            count: 2,
            unread: 1,
            depth,
        }
    }

    fn att(index: usize, name: &str, method: &'static str, size: i64) -> Attachment {
        Attachment {
            index,
            name: Some(name.into()),
            size: Some(size),
            mime_type: Some("application/SECRET_MIME".into()),
            content_id: Some("SECRET_CID@example.org".into()),
            method,
            inline: false,
            nid: 0,
        }
    }

    impl Source for Fake {
        fn folders(&mut self) -> Result<Vec<Folder>> {
            Ok(vec![
                folder(1, "SECRET_FOLDER_Alice", Some("IPF.Note"), 0),
                folder(2, "SECRET_CONTACTS", Some("IPF.Contact"), 1),
            ])
        }
        fn rows(&mut self, folder: Nid) -> Result<Vec<Row>> {
            assert_eq!(folder, 1, "only mail folders are scanned");
            // 2026-01-01 and 2025-06-01 (FILETIME).
            Ok(vec![
                Row {
                    nid: 10,
                    class: Some("IPM.Note".into()),
                    received: 134116992000000000,
                    flags: 0x11,
                    has_message_id: true,
                },
                Row {
                    nid: 11,
                    class: Some("IPM.Note.SECRET_Alice_Form".into()),
                    received: 133932960000000000,
                    flags: 0,
                    has_message_id: false,
                },
                Row {
                    nid: 12,
                    class: Some("IPM.Schedule.Meeting.Request".into()),
                    received: 0,
                    flags: 1,
                    has_message_id: true,
                },
            ])
        }
        fn sample(&mut self, nid: Nid) -> Result<Sample> {
            match nid {
                10 => Ok(Sample {
                    text: Some("Hello Alice, SECRET_BODY from bob@example.org. Quarterly plan.".repeat(60)),
                    text_time: Duration::from_millis(2),
                    attachments: vec![
                        att(0, "SECRET_ATTACH_Alice.PDF", "by_value", 1_000),
                        att(1, "Alice notes.SECRETX", "by_value", 30_000_000),
                        att(2, "SECRET fwd", "embedded_message", 500),
                    ],
                }),
                11 => Err(Error::Corrupt("SECRET_ERR Alice".into())),
                _ => Ok(Sample { text: None, text_time: Duration::ZERO, attachments: vec![] }),
            }
        }
    }

    #[test]
    fn doctor_output_has_no_mail_content() {
        let v = report(&mut Fake, 10, 0);
        let s = v.to_string();
        for m in MARKERS {
            assert!(!s.contains(m), "doctor output leaks {m:?}: {s}");
        }
        assert_eq!(v["folders"]["total"], 2);
        assert_eq!(v["folders"]["mail"], 1);
        assert_eq!(v["folders"]["by_class"]["IPF.Contact"], 1);
        assert_eq!(v["messages"]["total"], 3);
        assert_eq!(v["messages"]["unread"], 1);
        assert_eq!(v["messages"]["with_attachments"], 1);
        assert_eq!(v["messages"]["with_message_id"], 2);
        assert_eq!(v["messages"]["without_message_id"], 1);
        assert_eq!(v["messages"]["by_class"]["IPM.Note"], 2);
        assert_eq!(v["messages"]["by_class"]["IPM.Schedule"], 1);
        assert_eq!(v["folders"]["by_class"]["IPF.Note"], 1);
        assert_eq!(v["messages"]["by_year"]["2026"], 1);
        assert_eq!(v["messages"]["by_year"]["2025"], 1);
        assert_eq!(v["messages"]["by_year"]["unknown"], 1);
        assert_eq!(v["sample"]["requested"], 10);
        assert_eq!(v["sample"]["read"], 2);
        assert_eq!(v["sample"]["no_text"], 1);
        assert_eq!(v["attachments"]["count"], 3);
        assert_eq!(v["attachments"]["by_extension"]["pdf"], 1);
        assert_eq!(v["attachments"]["by_extension"]["other"], 1);
        assert_eq!(v["attachments"]["by_extension"]["none"], 1);
        assert_eq!(v["attachments"]["by_method"]["embedded_message"], 1);
        assert_eq!(v["attachments"]["over_20mb"], 1);
        assert_eq!(v["errors"]["inconsistent"], 1);
        // Sizes 500, 1_000, 30_000_000: nearest-rank p50 = 1_000, p95 = 30_000_000.
        let b = &v["attachments"]["bytes"];
        assert_eq!(
            (b["min"].as_u64(), b["p50"].as_u64(), b["p95"].as_u64()),
            (Some(500), Some(1_000), Some(30_000_000))
        );
        for d in ["open_text_us", "read_us", "text_us", "text_chars"] {
            let d = &v["sample"][d];
            assert!(d["p95"].is_u64() && d.get("p90").is_none(), "{d}");
        }
        #[cfg(any(target_os = "linux", windows))]
        assert!(v["machine"]["ram_bytes"].as_u64().is_some_and(|b| b > 0));
    }

    #[test]
    fn dist_percentiles() {
        assert_eq!(dist(vec![]), Json::Null);
        assert_eq!(dist(vec![7]), json!({ "min": 7, "p50": 7, "p95": 7, "max": 7, "sum": 7 }));
        let v: Vec<u64> = (1..=100).rev().collect();
        assert_eq!(dist(v), json!({ "min": 1, "p50": 50, "p95": 95, "max": 100, "sum": 5050 }));
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(dist(v)["p95"], 10);
    }

    #[test]
    fn class_prefixes() {
        let c = |s| class_prefix(Some(s));
        assert_eq!(c("IPM.Note"), "IPM.Note");
        assert_eq!(c("IPM.Note.Custom"), "IPM.Note");
        assert_eq!(c("ipm.note.SMIME.MultipartSigned"), "IPM.Note");
        assert_eq!(c("IPF.Note.OutlookHomepage"), "IPF.Note");
        assert_eq!(c("IPM.Schedule.Meeting.Request"), "IPM.Schedule");
        assert_eq!(c("REPORT.IPM.Note.NDR"), "REPORT.IPM");
        assert_eq!(c("IPM.SECRET_Alice"), "IPM.other");
        assert_eq!(c("SECRET.Note"), "other.Note");
        assert_eq!(c("IPM"), "IPM");
        assert_eq!(class_prefix(None), "none");
    }

    #[test]
    fn random_sample() {
        let a = pick(1000, 50, 42);
        assert_eq!(a, pick(1000, 50, 42), "same seed, same sample");
        assert_ne!(a, pick(1000, 50, 43));
        let mut s = a.clone();
        s.sort_unstable();
        s.dedup();
        assert_eq!(s.len(), 50, "distinct");
        assert!(a.iter().all(|&i| i < 1000));
        assert_ne!(s, (0..50).collect::<Vec<_>>(), "not the first rows");
        let mut all = pick(5, 10, 1);
        all.sort_unstable();
        assert_eq!(all, [0, 1, 2, 3, 4], "sample larger than the mailbox takes everything");
        assert!(pick(0, 10, 1).is_empty());
    }

    #[test]
    fn total_ram_from_meminfo() {
        assert_eq!(meminfo_total("MemTotal:       16314312 kB\nMemFree: 1 kB\n"), Some(16314312 * 1024));
        assert_eq!(meminfo_total("MemFree: 1 kB\n"), None);
        assert_eq!(meminfo_total("MemTotal: lots\n"), None);
    }

    #[test]
    fn errors_are_sanitized() {
        let m = || "/home/SECRET_USER/Alice.ost: SECRET_BODY".to_string();
        for e in [Error::Io(m()), Error::Format(m()), Error::Corrupt(m()), Error::NotFound(m()), Error::Usage(m())] {
            let (kind, code) = (e.kind(), e.exit_code());
            let s = sanitize(e);
            assert_eq!((s.kind(), s.exit_code()), (kind, code));
            assert!(!s.to_string().contains("SECRET"), "{s}");
        }
        let e = run(Path::new("/nonexistent/SECRET_USER/Alice.ost"), Path::new("/x"), 1).unwrap_err();
        assert_eq!(e.kind(), "io");
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }

    #[test]
    fn chunk_estimate_steps() {
        assert_eq!(chunks(0), 0);
        assert_eq!(chunks(1), 1);
        assert_eq!(chunks(CHUNK_CHARS), 1);
        assert_eq!(chunks(CHUNK_CHARS + 1), 2);
        assert_eq!(chunks(CHUNK_CHARS + CHUNK_CHARS - CHUNK_OVERLAP), 2);
    }
}
