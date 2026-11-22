//! Attachment text extraction: PDF text layer, DOCX, PPTX, XLSX, TXT, CSV,
//! HTML. Pure Rust. Parsers see untrusted files: `extract` turns a panic
//! into `failed`, and `Isolated` runs it in a child process
//! (`outlook-index extract-worker`) killed after a per-file timeout.

use std::borrow::Cow;
use std::io::{BufReader, Cursor, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use quick_xml::events::{BytesStart, Event};
use serde::{Deserialize, Serialize};

use super::worker::{read_frame, write_frame};
use crate::body::{decode_text, html_to_text, tidy, truncate};
use crate::error::{Error, Result};
use crate::store::Attachment;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Txt,
    Csv,
    Html,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Pdf => "pdf",
            Kind::Docx => "docx",
            Kind::Xlsx => "xlsx",
            Kind::Pptx => "pptx",
            Kind::Txt => "txt",
            Kind::Csv => "csv",
            Kind::Html => "html",
        }
    }
}

/// Why an attachment gives no text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    Image,
    Inline,
    TooLarge,
    UnsupportedType,
    EmbeddedMessage,
    Encrypted,
    /// No text layer (scanned PDF) or blank document.
    EmptyText,
    /// Malformed file, parser panic or worker crash.
    Failed,
    Timeout,
}

impl Skip {
    pub fn as_str(self) -> &'static str {
        match self {
            Skip::Image => "image",
            Skip::Inline => "inline",
            Skip::TooLarge => "too_large",
            Skip::UnsupportedType => "unsupported_type",
            Skip::EmbeddedMessage => "embedded_message",
            Skip::Encrypted => "encrypted",
            Skip::EmptyText => "empty_text",
            Skip::Failed => "failed",
            Skip::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    pub kind: Kind,
    pub text: String,
    /// The text was cut at a limit.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Larger files are skipped (too_large).
    pub max_bytes: usize,
    /// Output text cap, in characters.
    pub max_chars: usize,
    /// XLSX: rows per sheet, and cells in the whole workbook.
    pub max_rows: usize,
    pub max_cells: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_bytes: 20 << 20, max_chars: 2_000_000, max_rows: 20_000, max_cells: 200_000 }
    }
}

/// Default per-file timeout of `Isolated`.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Decompressed bytes read from one zip part (zip bomb guard).
const MAX_PART: u64 = 256 << 20;

pub type Outcome = std::result::Result<Extracted, Skip>;

const IMAGES: &[&str] =
    &["png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "svg", "heic", "webp", "ico", "emf", "wmf"];

fn by_ext(name: Option<&str>) -> Option<Typed> {
    let e = name.and_then(|n| Path::new(n).extension())?.to_str()?.to_ascii_lowercase();
    Some(Ok(match e.as_str() {
        "pdf" => Kind::Pdf,
        "docx" | "docm" => Kind::Docx,
        "xlsx" | "xlsm" => Kind::Xlsx,
        "pptx" | "pptm" => Kind::Pptx,
        "txt" | "text" => Kind::Txt,
        "csv" => Kind::Csv,
        "htm" | "html" => Kind::Html,
        e if IMAGES.contains(&e) => return Some(Err(Skip::Image)),
        _ => return None,
    }))
}

fn by_mime(mime: Option<&str>) -> Option<Typed> {
    let m = mime?.split(';').next()?.trim().to_ascii_lowercase();
    const OOXML: &str = "application/vnd.openxmlformats-officedocument.";
    Some(Ok(match m.strip_prefix(OOXML).unwrap_or(&m) {
        "application/pdf" => Kind::Pdf,
        "wordprocessingml.document" => Kind::Docx,
        "spreadsheetml.sheet" => Kind::Xlsx,
        "presentationml.presentation" => Kind::Pptx,
        "text/plain" => Kind::Txt,
        "text/csv" => Kind::Csv,
        "text/html" => Kind::Html,
        m if m.starts_with("image/") => return Some(Err(Skip::Image)),
        _ => return None,
    }))
}

/// A kind, or the reason to skip.
type Typed = std::result::Result<Kind, Skip>;
type Zip<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

/// Kind from magic bytes: %PDF, images, PK zip with an Office main part.
fn by_magic(b: &[u8]) -> Option<Typed> {
    if b[..b.len().min(1024)].windows(5).any(|w| w == b"%PDF-") {
        return Some(Ok(Kind::Pdf));
    }
    const IMG: &[&[u8]] = &[b"\x89PNG", b"\xFF\xD8\xFF", b"GIF8", b"II*\0", b"MM\0*"];
    if IMG.iter().any(|m| b.starts_with(m)) {
        return Some(Err(Skip::Image));
    }
    if !b.starts_with(b"PK\x03\x04") {
        return None;
    }
    let z = zip::ZipArchive::new(Cursor::new(b)).ok()?;
    let has = |n: &str| z.index_for_name(n).is_some();
    let k = [("word/document.xml", Kind::Docx), ("xl/workbook.xml", Kind::Xlsx), ("ppt/presentation.xml", Kind::Pptx)]
        .into_iter()
        .find(|(n, _)| has(n))?;
    Some(Ok(k.1))
}

/// Type from the extension first, then magic bytes, then the MIME type.
fn detect(b: &[u8], name: Option<&str>, mime: Option<&str>) -> Typed {
    by_ext(name).or_else(|| by_magic(b)).or_else(|| by_mime(mime)).unwrap_or(Err(Skip::UnsupportedType))
}

/// Skip decided from the attachment's properties alone, before reading its data.
pub fn skip_attachment(a: &Attachment, l: &Limits) -> Option<Skip> {
    let (name, mime) = (a.name.as_deref(), a.mime_type.as_deref());
    if a.method == "embedded_message" {
        Some(Skip::EmbeddedMessage)
    } else if a.inline {
        Some(Skip::Inline)
    } else if let Some(Err(s)) = by_ext(name).or_else(|| by_mime(mime)) {
        Some(s)
    } else if a.size.is_some_and(|s| s > l.max_bytes as i64) {
        Some(Skip::TooLarge)
    } else if a.method != "by_value" {
        Some(Skip::UnsupportedType)
    } else {
        None
    }
}

/// Kind from the name and MIME type only ("image", "other" when unknown);
/// a fixed label, never part of the name.
pub fn label(name: Option<&str>, mime: Option<&str>) -> &'static str {
    match by_ext(name).or_else(|| by_mime(mime)) {
        Some(Ok(k)) => k.as_str(),
        Some(Err(_)) => "image",
        None => "other",
    }
}

/// Text of one attachment, in this process, with default limits.
pub fn extract(bytes: &[u8], name: Option<&str>, mime: Option<&str>) -> Outcome {
    extract_with(bytes, name, mime, &Limits::default())
}

pub fn extract_with(bytes: &[u8], name: Option<&str>, mime: Option<&str>, l: &Limits) -> Outcome {
    run(check(bytes, name, mime, l)?, bytes, l)
}

/// Size and type checks; a panic is `failed`.
fn check(bytes: &[u8], name: Option<&str>, mime: Option<&str>, l: &Limits) -> Typed {
    if bytes.len() > l.max_bytes {
        return Err(Skip::TooLarge);
    }
    catch_unwind(|| detect(bytes, name, mime)).unwrap_or(Err(Skip::Failed))
}

/// Parse as `kind`, normalise, cap.
fn run(kind: Kind, bytes: &[u8], l: &Limits) -> Outcome {
    let mut out = Out { s: String::new(), max: l.max_chars.saturating_mul(4).max(1), truncated: false };
    catch_unwind(AssertUnwindSafe(|| parse(kind, bytes, l, &mut out))).unwrap_or(Err(Skip::Failed))?;
    let (text, cut) = truncate(normalize(&out.s), l.max_chars);
    if text.is_empty() {
        return Err(Skip::EmptyText);
    }
    Ok(Extracted { kind, text, truncated: out.truncated || cut })
}

/// Raw output, stopped at `max` bytes (the char cap applies after
/// normalisation); `truncated` when any limit cut it.
struct Out {
    s: String,
    max: usize,
    truncated: bool,
}

impl Out {
    fn push(&mut self, t: &str) {
        self.s.push_str(t);
        self.truncated |= self.full();
    }
    fn full(&self) -> bool {
        self.s.len() >= self.max
    }
}

/// Control characters become spaces, runs of spaces one space (tabs kept
/// as cell separators), line ends trimmed, blank lines collapsed.
fn normalize(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for line in s.lines() {
        let line: String = line.chars().map(|c| if c.is_control() && c != '\t' { ' ' } else { c }).collect();
        for (i, cell) in line.split('\t').enumerate() {
            if i > 0 {
                o.push('\t');
            }
            for (j, w) in cell.split_whitespace().enumerate() {
                if j > 0 {
                    o.push(' ');
                }
                o.push_str(w);
            }
        }
        o.push('\n');
    }
    tidy(&o)
}

fn parse(kind: Kind, b: &[u8], l: &Limits, out: &mut Out) -> std::result::Result<(), Skip> {
    if matches!(kind, Kind::Docx | Kind::Xlsx | Kind::Pptx) {
        // Password-protected OOXML is an OLE compound file, not a zip.
        if b.starts_with(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1") {
            return Err(Skip::Encrypted);
        }
        let mut z = zip::ZipArchive::new(Cursor::new(b)).map_err(|_| Skip::Failed)?;
        return match kind {
            Kind::Docx => docx(&mut z, out),
            Kind::Pptx => pptx(&mut z, out),
            _ => xlsx(&mut z, l, out),
        };
    }
    match kind {
        Kind::Pdf => return pdf(b, out),
        Kind::Html => out.push(&html_to_text(&decode(b))),
        _ => out.push(&decode(b)),
    }
    Ok(())
}

/// UTF-8 (BOM optional), UTF-16 with BOM, else Windows-1252.
fn decode(b: &[u8]) -> String {
    match b {
        [0xEF, 0xBB, 0xBF, r @ ..] => String::from_utf8_lossy(r).into_owned(),
        [0xFF, 0xFE, r @ ..] => decode_text(r, Some(1200)),
        [0xFE, 0xFF, r @ ..] => {
            String::from_utf16_lossy(&r.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect::<Vec<_>>())
        }
        _ => decode_text(b, None),
    }
}

/// Text layer, pages separated by a blank line.
fn pdf(b: &[u8], out: &mut Out) -> std::result::Result<(), Skip> {
    let encrypted = || b.windows(8).any(|w| w == b"/Encrypt");
    let mut doc = lopdf::Document::load_mem(b).map_err(|_| if encrypted() { Skip::Encrypted } else { Skip::Failed })?;
    if doc.is_encrypted() && doc.decrypt("").is_err() {
        return Err(Skip::Encrypted);
    }
    let pages: Vec<u32> = doc.get_pages().into_keys().collect();
    let mut ok = 0;
    for p in &pages {
        let mut s = String::new();
        if pdf_extract::output_doc_page(&doc, &mut pdf_extract::PlainTextOutput::new(&mut s), *p).is_ok() {
            ok += 1;
            out.push(&s);
            out.push("\n\n");
        }
        if out.full() {
            break;
        }
    }
    if ok == 0 { Err(Skip::Failed) } else { Ok(()) }
}

/// Text of a character or predefined entity reference.
fn text<'e>(e: &'e Event) -> Option<Cow<'e, str>> {
    match e {
        Event::Text(t) => Some(t.xml10_content()),
        Event::CData(t) => Some(t.xml10_content()),
        Event::GeneralRef(r) => {
            if let Ok(Some(c)) = r.resolve_char_ref() {
                return Some(Cow::Owned(c.to_string()));
            }
            Some(Cow::Borrowed(match r.xml10_content().as_ref() {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "quot" => "\"",
                "apos" => "'",
                _ => return None,
            }))
        }
        _ => None,
    }
}

/// Feed the events of zip part `name` to `f` until it returns false or the
/// part ends. A missing part or broken XML is `failed`.
fn walk(z: &mut Zip, name: &str, mut f: impl FnMut(&Event) -> bool) -> std::result::Result<(), Skip> {
    let part = z.by_name(name).map_err(|_| Skip::Failed)?;
    let mut r = quick_xml::Reader::from_reader(BufReader::new(part.take(MAX_PART)));
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(|_| Skip::Failed)? {
            Event::Eof => return Ok(()),
            e if !f(&e) => return Ok(()),
            _ => buf.clear(),
        }
    }
}

/// Local name of a start, empty or end tag.
fn tag<'e>(e: &'e Event) -> Option<(&'e str, bool, bool)> {
    match e {
        Event::Start(s) => Some((s.local_name().into_inner(), true, false)),
        Event::Empty(s) => Some((s.local_name().into_inner(), true, true)),
        Event::End(s) => Some((s.local_name().into_inner(), false, true)),
        _ => None,
    }
}

fn attr(s: &BytesStart, key: &str) -> Option<String> {
    let a = s.attributes().flatten().find(|a| a.key.local_name().as_ref() == key)?;
    a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(Cow::into_owned)
}

/// Paragraph text (`<a:t>` / `<w:t>`), one line per paragraph.
fn paragraphs(z: &mut Zip, name: &str, t: &str, p: &str, out: &mut Out) -> std::result::Result<(), Skip> {
    let mut in_t = false;
    walk(z, name, |e| {
        match tag(e) {
            Some((n, open, close)) if n == t => in_t = open && !close,
            Some((n, _, true)) if n == p || n == "br" || n == "cr" => out.push("\n"),
            Some(("tab", true, _)) => out.push(" "),
            Some(_) => {}
            None if in_t => {
                if let Some(s) = text(e) {
                    out.push(&s)
                }
            }
            None => {}
        }
        !out.full()
    })
}

fn docx(z: &mut Zip, out: &mut Out) -> std::result::Result<(), Skip> {
    paragraphs(z, "word/document.xml", "t", "p", out)
}

/// Slides in number order, separated by a blank line.
// ponytail: number order, not presentation.xml's sldIdLst; reordered decks keep file order.
fn pptx(z: &mut Zip, out: &mut Out) -> std::result::Result<(), Skip> {
    let mut slides: Vec<(u32, String)> = z
        .file_names()
        .flatten()
        .filter_map(|n| {
            let k = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?.parse().ok()?;
            Some((k, n.to_string()))
        })
        .collect();
    slides.sort();
    if slides.is_empty() {
        return Err(Skip::Failed);
    }
    for (_, n) in slides {
        paragraphs(z, &n, "t", "p", out)?;
        out.push("\n\n");
        if out.full() {
            break;
        }
    }
    Ok(())
}

/// Sheet name line, then one line per row, cells tab-separated.
fn xlsx(z: &mut Zip, l: &Limits, out: &mut Out) -> std::result::Result<(), Skip> {
    // Shared strings: <si> = concatenated <t>, phonetic runs (<rPh>) excluded.
    let mut shared = Vec::new();
    if z.index_for_name("xl/sharedStrings.xml").is_some() {
        let (mut cur, mut in_t, mut rph, mut size) = (String::new(), false, false, 0);
        walk(z, "xl/sharedStrings.xml", |e| {
            match tag(e) {
                Some(("si", false, true)) => {
                    size += cur.len();
                    shared.push(std::mem::take(&mut cur))
                }
                Some(("rPh", open, close)) => rph = open && !close,
                Some(("t", open, close)) => in_t = open && !close,
                Some(_) => {}
                None if in_t && !rph => cur.extend(text(e)),
                None => {}
            }
            size < out.max
        })?;
    }
    let mut sheets = Vec::new();
    walk(z, "xl/workbook.xml", |e| {
        if let Event::Start(s) | Event::Empty(s) = e
            && s.local_name().as_ref() == "sheet"
        {
            sheets.push((attr(s, "name").unwrap_or_default(), attr(s, "id").unwrap_or_default()));
        }
        true
    })?;
    let mut targets = std::collections::HashMap::new();
    if z.index_for_name("xl/_rels/workbook.xml.rels").is_some() {
        walk(z, "xl/_rels/workbook.xml.rels", |e| {
            if let Event::Start(s) | Event::Empty(s) = e
                && let (Some(id), Some(t)) = (attr(s, "Id"), attr(s, "Target"))
            {
                targets.insert(id, t.strip_prefix('/').map_or_else(|| format!("xl/{t}"), str::to_string));
            }
            true
        })?;
    }
    let mut cells = 0;
    for (i, (name, id)) in sheets.iter().enumerate() {
        let part = targets.get(id).cloned().unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", i + 1));
        out.push(name);
        out.push("\n");
        let (mut rows, mut row, mut val, mut typ, mut in_v) = (0, Vec::<String>::new(), String::new(), None, false);
        walk(z, &part, |e| {
            match tag(e) {
                Some(("row", true, _)) if rows >= l.max_rows => {
                    out.truncated = true;
                    return false;
                }
                Some(("row", _, true)) => {
                    if row.iter().any(|c| !c.is_empty()) {
                        out.push(&row.join("\t"));
                        out.push("\n");
                        rows += 1;
                    }
                    row.clear();
                }
                Some(("c", open, close)) => {
                    if open && let Event::Start(s) | Event::Empty(s) = e {
                        typ = attr(s, "t");
                    }
                    if close {
                        let v = std::mem::take(&mut val);
                        let v = match typ.as_deref() {
                            Some("s") => {
                                v.trim().parse::<usize>().ok().and_then(|i| shared.get(i)).cloned().unwrap_or_default()
                            }
                            Some("b") => (if v.trim() == "1" { "TRUE" } else { "FALSE" }).to_string(),
                            _ => v,
                        };
                        row.push(v.replace(['\t', '\n', '\r'], " "));
                        cells += 1;
                        if cells >= l.max_cells {
                            out.push(&row.join("\t"));
                            out.truncated = true;
                            return false;
                        }
                    }
                }
                Some(("v" | "t", open, close)) => in_v = open && !close,
                Some(_) => {}
                None if in_v => val.extend(text(e)),
                None => {}
            }
            !out.full()
        })?;
        if out.full() || cells >= l.max_cells {
            break;
        }
        out.push("\n\n");
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Request {
    kind: Kind,
    limits: Limits,
}

/// Child side (`extract-worker`): one request = a JSON header frame
/// `{kind, limits}` then a data frame; one reply = JSON `{"Ok":..}` or
/// `{"Err":"reason"}`. Ends at end of input.
pub fn serve(r: &mut impl Read, w: &mut impl Write) -> Result<()> {
    while let Some(h) = read_frame(r)? {
        let data = read_frame(r)?.ok_or_else(|| Error::Io("extract worker: missing data frame".into()))?;
        let reply = match serde_json::from_slice::<Request>(&h) {
            Ok(q) => run(q.kind, &data, &q.limits),
            Err(_) => Err(Skip::Failed),
        };
        drop(data);
        write_frame(w, &serde_json::to_vec(&reply).map_err(|e| Error::Io(e.to_string()))?)?;
    }
    Ok(())
}

/// One child process; its pipes are served by threads so a hung child
/// cannot block the parent.
struct Worker {
    proc: Child,
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

fn spawn(cmd: &mut Command) -> std::io::Result<Worker> {
    // stderr discarded: nothing from a parsed file reaches the parent's output.
    let mut proc = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let (mut stdin, stdout) = (proc.stdin.take().unwrap(), proc.stdout.take().unwrap());
    let (tx, requests) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for f in requests {
            if write_frame(&mut stdin, &f).is_err() {
                break;
            }
        }
    });
    let (replies, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        while let Ok(Some(f)) = read_frame(&mut r) {
            if replies.send(f).is_err() {
                break;
            }
        }
    });
    Ok(Worker { proc, tx, rx })
}

/// Parent side: extraction in a child process, restarted after a timeout or
/// a crash. Type and size checks run here; the child gets the kind and the
/// bytes, never the name.
pub struct Isolated {
    pub limits: Limits,
    pub timeout: Duration,
    /// Child processes started so far.
    pub spawned: usize,
    cmd: Command,
    worker: Option<Worker>,
}

impl Isolated {
    pub fn new(cmd: Command, timeout: Duration, limits: Limits) -> Isolated {
        Isolated { limits, timeout, spawned: 0, cmd, worker: None }
    }

    pub fn extract(&mut self, bytes: &[u8], name: Option<&str>, mime: Option<&str>) -> Outcome {
        let kind = check(bytes, name, mime, &self.limits)?;
        if self.worker.is_none() {
            self.worker = Some(spawn(&mut self.cmd).map_err(|_| Skip::Failed)?);
            self.spawned += 1;
        }
        let w = self.worker.as_ref().unwrap();
        let header = serde_json::to_vec(&Request { kind, limits: self.limits }).map_err(|_| Skip::Failed)?;
        let reply = match w.tx.send(header).and_then(|_| w.tx.send(bytes.to_vec())) {
            Ok(()) => w.rx.recv_timeout(self.timeout),
            Err(_) => Err(RecvTimeoutError::Disconnected),
        };
        let r = match reply {
            Ok(f) => serde_json::from_slice(&f).map_err(|_| Skip::Failed),
            Err(RecvTimeoutError::Timeout) => Err(Skip::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Skip::Failed),
        };
        match r {
            Ok(r) => r,
            // Hung, dead or out of sync: kill; the next file starts a new child.
            Err(s) => {
                self.worker = None;
                Err(s)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Cursor, Write};
    use std::time::Instant;

    use super::*;

    /// A minimal PDF with one Helvetica text line per page.
    pub fn pdf(pages: &[&str]) -> Vec<u8> {
        let n = pages.len();
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        for (i, t) in pages.iter().enumerate() {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * i
            ));
            let s = if t.is_empty() { String::new() } else { format!("BT /F1 12 Tf 72 720 Td ({t}) Tj ET") };
            objs.push(format!("<< /Length {} >>\nstream\n{s}\nendstream", s.len()));
        }
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for o in offsets {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).bytes());
        out
    }

    pub fn zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut z = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in files {
            z.start_file(*name, ::zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    const CT: &str =
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#;

    pub fn docx(paras: &[&str]) -> Vec<u8> {
        let body: String = paras.iter().map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>")).collect();
        let doc = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
        );
        zip(&[("[Content_Types].xml", CT), ("word/document.xml", &doc)])
    }

    fn slide(texts: &[&str]) -> String {
        let ps: String = texts.iter().map(|t| format!("<a:p><a:r><a:t>{t}</a:t></a:r></a:p>")).collect();
        format!(
            r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody>{ps}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
    }

    fn xlsx(rows: usize) -> Vec<u8> {
        let wb = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="ws" Target="worksheets/sheet2.xml"/><Relationship Id="rId1" Type="ws" Target="/xl/worksheets/sheet1.xml"/></Relationships>"#;
        let ss = r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>Item</t></si><si><r><t>Cost</t></r><r><t> (EUR)</t></r></si><si><t>Paper &amp; ink</t></si></sst>"#;
        let mut s1 = String::from(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>"#,
        );
        for i in 0..rows {
            s1.push_str(&format!(r#"<row><c t="s"><v>2</v></c><c><f>1+1</f><v>{i}</v></c></row>"#));
        }
        s1.push_str("</sheetData></worksheet>");
        let s2 = r#"<worksheet><sheetData><row><c t="inlineStr"><is><t>Fictitious note</t></is></c><c t="b"><v>1</v></c></row></sheetData></worksheet>"#;
        zip(&[
            ("[Content_Types].xml", CT),
            ("xl/workbook.xml", wb),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/sharedStrings.xml", ss),
            ("xl/worksheets/sheet1.xml", &s1),
            ("xl/worksheets/sheet2.xml", s2),
        ])
    }

    fn ok(r: Outcome) -> Extracted {
        r.unwrap_or_else(|s| panic!("skipped: {s:?}"))
    }

    #[test]
    fn pdf_text_layer_with_page_breaks() {
        let e = ok(extract(&pdf(&["Fictitious invoice 42", "Second page text"]), Some("a.pdf"), None));
        assert_eq!(e.kind, Kind::Pdf);
        assert_eq!(e.text, "Fictitious invoice 42\n\nSecond page text", "{e:?}");
        assert!(!e.truncated);
        // Magic bytes, no name; MIME type only.
        assert_eq!(ok(extract(&pdf(&["x"]), None, None)).kind, Kind::Pdf);
        assert_eq!(ok(extract(&pdf(&["x"]), Some("scan"), Some("application/pdf"))).kind, Kind::Pdf);
        assert_eq!(extract(&pdf(&[""]), Some("scan.pdf"), None), Err(Skip::EmptyText), "no text layer");
    }

    #[test]
    fn docx_paragraphs_in_order() {
        let e = ok(extract(&docx(&["First paragraph", "Second &amp; last"]), Some("Report.DOCX"), None));
        assert_eq!((e.kind, e.text.as_str()), (Kind::Docx, "First paragraph\nSecond & last"));
        assert_eq!(ok(extract(&docx(&["x"]), None, None)).kind, Kind::Docx, "zip sniffed");
        assert_eq!(extract(&docx(&["", " "]), Some("blank.docx"), None), Err(Skip::EmptyText));
    }

    #[test]
    fn pptx_slides_in_number_order() {
        let p = zip(&[
            ("[Content_Types].xml", CT),
            ("ppt/presentation.xml", "<p:presentation/>"),
            ("ppt/slides/slide10.xml", &slide(&["Tenth"])),
            ("ppt/slides/slide2.xml", &slide(&["Second", "two b"])),
            ("ppt/slides/slide1.xml", &slide(&["Title slide"])),
            ("ppt/notesSlides/notesSlide1.xml", &slide(&["not a slide"])),
        ]);
        let e = ok(extract(&p, Some("deck.pptx"), None));
        assert_eq!((e.kind, e.text.as_str()), (Kind::Pptx, "Title slide\n\nSecond\ntwo b\n\nTenth"));
        assert_eq!(ok(extract(&p, None, None)).kind, Kind::Pptx);
    }

    #[test]
    fn xlsx_rows_sheets_and_caps() {
        let e = ok(extract(&xlsx(2), Some("b.xlsx"), None));
        assert_eq!(e.kind, Kind::Xlsx);
        assert_eq!(
            e.text, "Budget\nItem\tCost (EUR)\nPaper & ink\t0\nPaper & ink\t1\n\nNotes\nFictitious note\tTRUE",
            "{e:?}"
        );
        assert!(!e.truncated);
        assert_eq!(ok(extract(&xlsx(1), None, None)).kind, Kind::Xlsx);

        // A huge sheet is cut at the row cap, then the cell cap.
        let big = xlsx(5000);
        let l = Limits { max_rows: 10, ..Limits::default() };
        let e = ok(extract_with(&big, Some("b.xlsx"), None, &l));
        assert!(e.truncated);
        assert_eq!(e.text.lines().filter(|l| l.starts_with("Paper")).count(), 9, "{}", e.text);
        assert!(e.text.ends_with("Fictitious note\tTRUE"), "next sheet still read");
        let l = Limits { max_cells: 7, ..Limits::default() };
        let e = ok(extract_with(&big, Some("b.xlsx"), None, &l));
        assert!(e.truncated && !e.text.contains("Notes"), "{}", e.text);
        assert_eq!(e.text.matches('\t').count(), 3, "{}", e.text);
    }

    #[test]
    fn text_encodings() {
        let t = |b: &[u8]| ok(extract(b, Some("n.txt"), None)).text;
        assert_eq!(t("Café crème".as_bytes()), "Café crème");
        assert_eq!(t(&[b"\xEF\xBB\xBF".as_slice(), "Café".as_bytes()].concat()), "Café", "UTF-8 BOM");
        let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain("Été €".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(t(&le), "Été €", "UTF-16 LE BOM");
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("Été €".encode_utf16().flat_map(u16::to_be_bytes)).collect();
        assert_eq!(t(&be), "Été €", "UTF-16 BE BOM");
        assert_eq!(t(b"Caf\xE9 \x80 10"), "Café € 10", "Windows-1252 fallback");
        let e = ok(extract(b"a;b\r\n1;2\r\n", Some("data.csv"), None));
        assert_eq!((e.kind, e.text.as_str()), (Kind::Csv, "a;b\n1;2"));
        assert_eq!(ok(extract(b"plain", Some("x"), Some("text/plain"))).kind, Kind::Txt);
    }

    #[test]
    fn whitespace_normalized_and_text_capped() {
        let e = ok(extract(b"  a \t b\x00c  \r\n\r\n\r\n\r\n  d\xC2\xA0\xC2\xA0e  ", Some("x.txt"), None));
        assert_eq!(e.text, "a\tb c\n\nd e");
        let l = Limits { max_chars: 10, ..Limits::default() };
        let e = ok(extract_with("ééééé ééééé ééééé".as_bytes(), Some("x.txt"), None, &l));
        assert_eq!((e.text.chars().count(), e.truncated), (10, true));
    }

    #[test]
    fn html_to_text() {
        let h = b"<html><head><style>p{}</style></head><body><p>Hello&nbsp;<b>world</b></p><script>x()</script><p>Bye</p></body></html>";
        let e = ok(extract(h, Some("page.htm"), None));
        assert_eq!((e.kind, e.text.as_str()), (Kind::Html, "Hello world\nBye"));
        assert_eq!(ok(extract(h, None, Some("text/html; charset=utf-8"))).kind, Kind::Html);
    }

    fn att(name: &str, mime: Option<&str>, method: &'static str, inline: bool, size: i64) -> Attachment {
        Attachment {
            index: 0,
            name: Some(name.into()),
            size: Some(size),
            mime_type: mime.map(str::to_string),
            content_id: None,
            method,
            inline,
            nid: 0,
        }
    }

    #[test]
    fn skips() {
        let l = Limits::default();
        let s = |a: Attachment| skip_attachment(&a, &l);
        assert_eq!(s(att("photo.JPG", None, "by_value", false, 10)), Some(Skip::Image));
        assert_eq!(s(att("x", Some("image/png"), "by_value", false, 10)), Some(Skip::Image));
        assert_eq!(s(att("logo.pdf", None, "by_value", true, 10)), Some(Skip::Inline));
        assert_eq!(s(att("fwd", None, "embedded_message", false, 10)), Some(Skip::EmbeddedMessage));
        assert_eq!(s(att("big.pdf", None, "by_value", false, 21 << 20)), Some(Skip::TooLarge));
        assert_eq!(s(att("link.pdf", None, "by_reference", false, 10)), Some(Skip::UnsupportedType));
        assert_eq!(s(att("a.pdf", None, "by_value", false, 20 << 20)), None);

        assert_eq!(extract(b"\x89PNG\r\n\x1a\n....", None, None), Err(Skip::Image), "magic");
        assert_eq!(extract(b"x", Some("a.png"), None), Err(Skip::Image));
        assert_eq!(extract(b"MZ..", Some("setup.exe"), None), Err(Skip::UnsupportedType));
        assert_eq!(extract(b"MZ..", None, Some("application/octet-stream")), Err(Skip::UnsupportedType));
        assert_eq!(extract(&zip(&[("a.txt", "x")]), Some("a.zip"), None), Err(Skip::UnsupportedType), "no archives");
        assert_eq!(extract(&zip(&[("a.txt", "x")]), None, None), Err(Skip::UnsupportedType));
        let l = Limits { max_bytes: 4, ..Limits::default() };
        assert_eq!(extract_with(b"hello", Some("a.txt"), None, &l), Err(Skip::TooLarge));
        // Password-protected OOXML is an OLE compound file.
        let cfb = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1 EncryptedPackage";
        assert_eq!(extract(cfb, Some("secret.docx"), None), Err(Skip::Encrypted));
        assert_eq!(extract(cfb, Some("old.doc"), None), Err(Skip::UnsupportedType));
        assert_eq!(extract(b"   \r\n ", Some("e.txt"), None), Err(Skip::EmptyText));

        assert_eq!((label(Some("A.PDF"), None), label(None, Some("image/gif"))), ("pdf", "image"));
        assert_eq!((label(Some("SECRET.weird"), None), label(None, None)), ("other", "other"));
    }

    #[test]
    fn malformed_files_fail() {
        for (b, n) in [
            (&b"PK\x03\x04 not a zip at all"[..], "a.docx"),
            (b"PK\x03\x04 not a zip", "a.xlsx"),
            (b"PK\x03\x04", "a.pptx"),
            (b"%PDF-1.4\n1 0 obj << /Type /Catalog >> garbage", "a.pdf"),
            (b"%PDF-", "a.pdf"),
        ] {
            assert_eq!(extract(b, Some(n), None), Err(Skip::Failed), "{n}");
        }
        // A valid zip without the main part.
        assert_eq!(extract(&zip(&[("x.xml", "<a/>")]), Some("a.docx"), None), Err(Skip::Failed));
        // Broken XML.
        let bad = zip(&[("word/document.xml", "<w:document><w:p><w:t>a</w:p></oops>")]);
        assert_eq!(extract(&bad, Some("a.docx"), None), Err(Skip::Failed));
        // Truncated PDF.
        let p = pdf(&["Some text"]);
        assert_eq!(extract(&p[..p.len() / 3], Some("a.pdf"), None), Err(Skip::Failed));
    }

    fn frames(reqs: &[(Kind, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        for (k, data) in reqs {
            let h = serde_json::json!({ "kind": k, "limits": Limits::default() }).to_string();
            crate::index::worker::write_frame(&mut b, h.as_bytes()).unwrap();
            crate::index::worker::write_frame(&mut b, data).unwrap();
        }
        b
    }

    #[test]
    fn worker_protocol() {
        let input = frames(&[(Kind::Txt, b"hello"), (Kind::Pdf, b"%PDF- broken"), (Kind::Docx, &docx(&["d"]))]);
        let mut out = Vec::new();
        serve(&mut input.as_slice(), &mut out).unwrap();
        let mut r = out.as_slice();
        let mut next = || -> Outcome {
            serde_json::from_slice(&crate::index::worker::read_frame(&mut r).unwrap().unwrap()).unwrap()
        };
        assert_eq!(next().unwrap().text, "hello");
        assert_eq!(next(), Err(Skip::Failed));
        assert_eq!(next().unwrap().kind, Kind::Docx);
        assert!(crate::index::worker::read_frame(&mut r).unwrap().is_none());
    }

    /// The parent's own checks need no child.
    #[test]
    fn isolated_checks_before_spawning() {
        let mut x = Isolated::new(Command::new("/nonexistent/worker"), TIMEOUT, Limits::default());
        assert_eq!(x.extract(b"x", Some("a.png"), None), Err(Skip::Image));
        assert_eq!(x.extract(b"x", Some("a.exe"), None), Err(Skip::UnsupportedType));
        assert_eq!(x.spawned, 0);
        assert_eq!(x.extract(b"x", Some("a.txt"), None), Err(Skip::Failed), "worker cannot start");
    }

    /// A worker that never answers is killed at the timeout and replaced.
    #[cfg(unix)]
    #[test]
    fn hanging_worker_times_out_and_restarts() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let mut x = Isolated::new(cmd, Duration::from_millis(300), Limits::default());
        let t = Instant::now();
        // Larger than a pipe buffer: writing must not block either.
        let big = vec![b'a'; 1 << 20];
        assert_eq!(x.extract(&big, Some("a.txt"), None), Err(Skip::Timeout));
        assert_eq!(x.extract(b"b", Some("b.txt"), None), Err(Skip::Timeout));
        assert_eq!(x.spawned, 2, "restarted after the timeout");
        assert!(t.elapsed() < Duration::from_secs(10), "killed, not waited for");
    }

    /// A worker that dies (crash, panic outside the guard) gives failed.
    #[cfg(unix)]
    #[test]
    fn crashing_worker_fails_and_restarts() {
        let mut x = Isolated::new(Command::new("true"), TIMEOUT, Limits::default());
        assert_eq!(x.extract(b"a", Some("a.txt"), None), Err(Skip::Failed));
        assert_eq!(x.extract(b"b", Some("b.txt"), None), Err(Skip::Failed));
        assert_eq!(x.spawned, 2);
    }
}
