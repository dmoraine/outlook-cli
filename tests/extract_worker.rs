//! Attachment extraction through the real `outlook-index extract-worker` child.
#![cfg(feature = "index")]

use std::process::Command;

use outlook_cli::index::extract::{Isolated, Kind, Limits, Skip, TIMEOUT};

#[test]
fn extract_worker_child() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_outlook-index"));
    cmd.arg("extract-worker");
    let mut x = Isolated::new(cmd, TIMEOUT, Limits::default());
    let e = x.extract("Fictitious note: café".as_bytes(), Some("SECRET_note.txt"), None).unwrap();
    assert_eq!((e.kind, e.text.as_str(), e.truncated), (Kind::Txt, "Fictitious note: café", false));
    assert_eq!(x.extract(b"%PDF-1.4 broken", Some("a.pdf"), None), Err(Skip::Failed));
    assert_eq!(x.extract(b"PK\x03\x04 broken", Some("a.docx"), None), Err(Skip::Failed));
    let e = x.extract(b"<p>Hello <b>world</b></p>", None, Some("text/html")).unwrap();
    assert_eq!((e.kind, e.text.as_str()), (Kind::Html, "Hello world"));
    assert_eq!(x.spawned, 1, "one child for every file, failures included");
}
