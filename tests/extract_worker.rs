//! Attachment extraction through the real `outlook-index extract-worker` child.
#![cfg(feature = "index")]

use std::io::{Cursor, Write};
use std::process::Command;

use outlook_cli::error::Result;
use outlook_cli::index::extract::{Isolated, Kind, Limits, MEMORY_ENFORCED, Skip, TIMEOUT};
use outlook_cli::index::sync::{self, Entry, MailSource, Opened};
use outlook_cli::index::{attach, migrate};
use outlook_cli::ndb::Nid;
use outlook_cli::store::{Attachment, Folder, MSGFLAG_HASATTACH, WellKnown};

const MIB: u64 = 1 << 20;

fn worker() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_outlook-index"));
    cmd.arg("extract-worker");
    cmd
}

#[test]
fn extract_worker_child() {
    let mut x = Isolated::new(worker(), TIMEOUT, Limits::default());
    let e = x.extract("Fictitious note: café".as_bytes(), Some("SECRET_note.txt"), None).unwrap();
    assert_eq!((e.kind, e.text.as_str(), e.truncated), (Kind::Txt, "Fictitious note: café", false));
    assert_eq!(x.extract(b"%PDF-1.4 broken", Some("a.pdf"), None), Err(Skip::Failed));
    assert_eq!(x.extract(b"PK\x03\x04 broken", Some("a.docx"), None), Err(Skip::Failed));
    let e = x.extract(b"<p>Hello <b>world</b></p>", None, Some("text/html")).unwrap();
    assert_eq!((e.kind, e.text.as_str()), (Kind::Html, "Hello world"));
    assert_eq!(x.spawned, 1, "one child for every file, failures included");
}

/// S10: the OS limit kills the child, not the parent: a test hook makes the
/// child allocate 512 MiB for a `TEST_ALLOC` file. Under a 256 MiB limit it
/// dies (`memory_limit`) and the next file gets a new child; without a
/// limit the same file is extracted.
#[test]
fn os_memory_limit_kills_only_the_child() {
    if !MEMORY_ENFORCED {
        return eprintln!("skipped: no OS memory limit on this target");
    }
    let mut cmd = worker();
    cmd.env("OUTLOOK_INDEX_TEST_EXTRACT_ALLOC", "512");
    let mut x = Isolated::new(cmd, TIMEOUT, Limits::default()).with_memory(256 * MIB);
    assert_eq!(x.memory, Some(256 * MIB));
    assert_eq!(x.extract(b"TEST_ALLOC", Some("a.txt"), None), Err(Skip::MemoryLimit));
    assert_eq!(x.extract(b"Fictitious", Some("b.txt"), None).unwrap().text, "Fictitious");
    assert_eq!(x.extract(b"TEST_ALLOC", Some("c.txt"), None), Err(Skip::MemoryLimit));
    assert_eq!(x.extract(b"Fictitious", Some("d.txt"), None).unwrap().text, "Fictitious");
    assert_eq!(x.spawned, 3, "restarted after each kill");

    let mut cmd = worker();
    cmd.env("OUTLOOK_INDEX_TEST_EXTRACT_ALLOC", "512");
    let mut x = Isolated::new(cmd, TIMEOUT, Limits::default());
    assert_eq!(x.extract(b"TEST_ALLOC", Some("a.txt"), None).unwrap().text, "TEST_ALLOC", "no limit: no kill");
}

fn zip(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in files {
        z.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(body).unwrap();
    }
    z.finish().unwrap().into_inner()
}

const DOC: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#;

/// A docx whose document.xml unpacks to 64 MiB of empty paragraphs.
fn zip_bomb() -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
    z.write_all(DOC.as_bytes()).unwrap();
    let block = "<w:p/>".repeat((1 << 20) / 6);
    for _ in 0..64 {
        z.write_all(block.as_bytes()).unwrap();
    }
    z.finish().unwrap().into_inner()
}

/// A docx with zips nested 8 deep under a 40-level entry path.
fn nested_zip() -> Vec<u8> {
    let mut nested = zip(&[("a.txt".into(), b"Fictitious".to_vec())]);
    for _ in 0..8 {
        nested = zip(&[("inner.zip".into(), nested)]);
    }
    let main = format!("{DOC}<w:p><w:r><w:t>Fictitious</w:t></w:r></w:p></w:body></w:document>");
    zip(&[("word/document.xml".into(), main.into_bytes()), (format!("{}x.zip", "d/".repeat(40)), nested)])
}

/// One-page PDF; `nulls` extra null objects; `bomb` MiB of spaces as the
/// deflated page content.
fn pdf(nulls: usize, bomb: usize) -> Vec<u8> {
    let content = if bomb == 0 {
        b"BT /F1 12 Tf 72 720 Td (Fictitious) Tj ET".to_vec()
    } else {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        let block = vec![b' '; 1 << 20];
        for _ in 0..bomb {
            z.write_all(&block).unwrap();
        }
        z.finish().unwrap()
    };
    let filter = if bomb == 0 { "" } else { " /Filter /FlateDecode" };
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        [format!("<< /Length {}{filter} >>\nstream\n", content.len()).as_bytes(), &content, b"\nendstream"].concat(),
    ];
    objs.extend(std::iter::repeat_n(b"null".to_vec(), nulls));
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n", i + 1).bytes());
        out.extend(o);
        out.extend(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).bytes());
    out
}

/// One fictitious message carrying `files`.
struct Mailbox {
    files: Vec<(Attachment, Vec<u8>)>,
}

impl MailSource for Mailbox {
    fn revision(&mut self) -> Result<String> {
        Ok("r1".into())
    }
    fn folders(&mut self) -> Result<Vec<Folder>> {
        let f = Folder {
            nid: 1,
            parent: None,
            name: "Inbox".into(),
            path: "Inbox".into(),
            class: Some("IPF.Note".into()),
            count: 1,
            unread: 0,
            depth: 0,
        };
        Ok(vec![f])
    }
    fn entries(&mut self, _: Nid) -> Result<Vec<Entry>> {
        Ok(vec![Entry {
            nid: 100,
            folder: 1,
            received: 134_000_000_000_000_000,
            flags: 1 | MSGFLAG_HASATTACH,
            subject: Some("Fictitious".into()),
            message_id: Some("<fictitious@example.org>".into()),
            ..Default::default()
        }])
    }
    fn open(&mut self, _: Nid) -> Result<Opened> {
        Ok(Opened { text: Some("Fictitious body".into()), ..Default::default() })
    }
    fn well_known(&mut self, _: &[Folder]) -> Result<WellKnown> {
        Ok(WellKnown::default())
    }
    fn attachments(&mut self, _: Nid) -> Result<Vec<Attachment>> {
        Ok(self.files.iter().map(|f| f.0.clone()).collect())
    }
    fn attachment_data(&mut self, _: Nid, a: &Attachment) -> Result<Vec<u8>> {
        Ok(self.files[a.index].1.clone())
    }
}

/// S10: hostile fictitious attachments (zip bomb, nested zip, PDF with a
/// huge object count, PDF decompression bomb) end as `too_complex` or
/// `memory_limit`; the parent lives on, every sync completes, the child
/// restarts, the next file is extracted, and `memory_limit` is retried
/// like a failure (3 attempts). Summaries carry counts and labels only.
#[test]
fn hostile_attachments_sync_completes() {
    if !MEMORY_ENFORCED {
        return eprintln!("skipped: no OS memory limit on this target");
    }
    let files = [
        ("SECRET_bomb.docx", zip_bomb()),
        ("SECRET_nested.docx", nested_zip()),
        ("SECRET_objects.pdf", pdf(250_000, 0)),
        ("SECRET_inflate.pdf", pdf(0, 384)),
        ("SECRET_note.txt", b"Fictitious note".to_vec()),
    ];
    let files = files
        .into_iter()
        .enumerate()
        .map(|(index, (name, data))| {
            assert!(data.len() < 20 << 20, "{name}: {}", data.len());
            let a = Attachment {
                index,
                name: Some(name.into()),
                size: Some(data.len() as i64),
                mime_type: None,
                content_id: None,
                method: "by_value",
                inline: false,
                nid: 0,
            };
            (a, data)
        })
        .collect();
    let mut src = Mailbox { files };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&mut c).unwrap();
    let mut x = Isolated::new(worker(), TIMEOUT, Limits::default()).with_memory(256 * MIB);
    let mut summaries = Vec::new();
    for _ in 0..3 {
        let mut f = |b: &[u8], n: Option<&str>, m: Option<&str>| x.extract(b, n, m);
        let s = sync::sync_with(&mut c, &mut src, Some(&mut f), &sync::Options::default()).unwrap();
        assert!(!s.to_string().contains("SECRET"), "{s}");
        summaries.push(s);
    }
    let s = &summaries[0];
    assert_eq!(s["attachments_skipped_by_reason"]["too_complex"], 3, "{s}");
    assert_eq!((s["attachments_failed"].as_u64(), s["attachments_extracted"].as_u64()), (Some(1), Some(1)), "{s}");
    assert_eq!(s["complete"], false, "memory_limit is retried: {s}");
    assert_eq!(summaries[1]["attachments_failed"], 1, "{}", summaries[1]);
    let last = &summaries[2];
    assert_eq!((last["attachments_remaining"].as_u64(), last["complete"].as_bool()), (Some(0), Some(true)), "{last}");
    let counts = attach::counts(&c).unwrap();
    assert_eq!(counts["by_outcome"], serde_json::json!({ "memory_limit": 1, "ok": 1, "too_complex": 3 }), "{counts}");
    assert!(!counts.to_string().contains("SECRET"), "{counts}");
    // Sync 1: killed by the PDF bomb, a new child for the note; syncs 2 and 3:
    // the bomb alone, retried, kills the child of the sync before and its own.
    assert_eq!(x.spawned, 3, "a new child after each kill");
}
