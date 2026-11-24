//! Public error boundary of `outlook-index`: marker strings in folder and
//! people filters, the query, times, the mailbox file name, the index path,
//! model directories, a child error frame and forced panics never reach
//! stdout or stderr; kinds and exit codes are kept. OUTLOOK_INDEX_DEBUG=1
//! adds the raw text as `error.debug` only.
#![cfg(feature = "index")]

use std::process::Command;

use serde_json::Value as Json;

const MARK: &str = "SECRET";

fn bin(args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_outlook-index"));
    c.args(args).env_remove("OUTLOOK_INDEX_DEBUG").env_remove("OUTLOOK_INDEX_TEST_PANIC");
    c.env_remove("OUTLOOK_INDEX_MODEL_DIR").env_remove("OUTLOOK_CLI_FILE").env_remove("OUTLOOK_INDEX_DB");
    c
}

/// Exit `code`, error `kind`, nothing on stdout, no marker anywhere, no
/// debug field; returns the error object.
fn fails(c: &mut Command, code: i32, kind: &str) -> Json {
    let o = c.output().unwrap();
    let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(!out.contains(MARK) && !err.contains(MARK), "leak: {c:?}\n{out}\n{err}");
    assert_eq!(o.status.code(), Some(code), "{c:?}: {err}");
    assert!(out.is_empty(), "{c:?}: {out}");
    let v: Json = serde_json::from_str(err.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{c:?}: not JSON ({e}): {err}"));
    assert_eq!(v["error"]["kind"], kind, "{c:?}: {err}");
    assert!(v["error"].get("debug").is_none(), "{err}");
    v["error"].clone()
}

#[test]
fn errors_leak_no_input_path_or_panic_text() {
    let dir = std::env::temp_dir().join(format!("outlook-index-{MARK}_DIR-{}", std::process::id()));
    let idx = dir.join(format!("{MARK}_IDX")).join("index.sqlite");
    let empty = dir.join(format!("{MARK}_MODELS"));
    std::fs::create_dir_all(&empty).unwrap();
    let ix = |args: &[&str]| {
        let mut c = bin(args);
        c.env("OUTLOOK_INDEX_DB", &idx).env("XDG_DATA_HOME", &dir).env("LOCALAPPDATA", &dir);
        c
    };

    fails(&mut ix(&["status"]), 3, "not_found");
    // One indexed folder: `scope -F` checks its specs once folders exist.
    let c = outlook_cli::index::open(&idx).unwrap();
    c.execute("INSERT INTO folders(nid, path, name) VALUES (1, 'Inbox', 'Inbox')", []).unwrap();
    drop(c);

    // Folder and people filters, query text.
    let search = ["search", "SECRET_QUERY", "-F", "SECRET_FOLDER", "--from", "SECRET_FROM", "--to", "SECRET_TO"];
    let e = fails(&mut ix(&search), 3, "not_found");
    assert!(e["message"].as_str().unwrap().contains("folder"), "{e}");
    fails(&mut ix(&["scope", "-F", "SECRET_FOLDER"]), 3, "not_found");
    fails(&mut ix(&["search", "SECRET_QUERY", "--since", "SECRET_WHEN"]), 2, "usage");
    fails(&mut ix(&["scope", "--embed-until", "SECRET_WHEN"]), 2, "usage");
    // Command-line errors from the parser: JSON, no user token.
    fails(&mut ix(&["search", "-SECRET_QUERY"]), 2, "usage");
    fails(&mut ix(&["SECRET_COMMAND"]), 2, "usage");
    let e = fails(&mut ix(&["search", "x", "--mode", "SECRET_MODE"]), 2, "usage");
    assert!(e["message"].as_str().unwrap().contains("keyword"), "allowed values: {e}");
    fails(&mut ix(&["sync", "--max-time", "SECRET_TIME", "--file", "x.ost"]), 2, "usage");

    // Mailbox file name.
    let ost = dir.join("SECRET_alice@example.org.ost");
    fails(ix(&["sync", "--no-embed", "--file"]).arg(&ost), 4, "io");
    fails(ix(&["doctor", "--file"]).arg(&ost), 4, "io");
    std::fs::write(&ost, b"SECRET_BODY not a pst".repeat(100)).unwrap();
    fails(ix(&["sync", "--no-embed", "--file"]).arg(&ost), 5, "format");

    // Model directory; model and bench commands.
    let e = fails(ix(&["embed", "--model-dir"]).arg(&empty), 3, "not_found");
    assert!(e["message"].as_str().unwrap().contains("e5-base"), "useful safe context: {e}");
    fails(ix(&["bench-embed", "--seconds", "1", "--model-dir"]).arg(&empty), 3, "not_found");
    #[cfg(unix)]
    fails(ix(&["model", "fetch", "--model-dir"]).arg(&empty).env("PATH", ""), 3, "not_found");

    // Index file that is not SQLite.
    let junk = dir.join("SECRET_junk.sqlite");
    std::fs::write(&junk, b"SECRET_BODY not sqlite".repeat(100)).unwrap();
    let mut c = bin(&["status", "--index"]);
    c.arg(&junk);
    fails(&mut c, 4, "io");

    // A child error frame: the embedding worker fails while loading.
    let mut c = ix(&["embed", "--workers", "2", "--model-dir"]);
    c.arg(&empty).env("OUTLOOK_INDEX_TEST_PANIC", "embed-worker SECRET_CHILD");
    fails(&mut c, 6, "inconsistent");

    // Forced panic on every public command: kind, exit code, code location only.
    for args in [
        &["doctor"][..],
        &["sync"],
        &["embed"],
        &["scope"],
        &["search", "x"],
        &["status"],
        &["model", "list"],
        &["model", "fetch"],
        &["bench-embed"],
    ] {
        let mut c = ix(args);
        c.env("OUTLOOK_INDEX_TEST_PANIC", "run SECRET_PANIC");
        let e = fails(&mut c, 6, "inconsistent");
        assert!(e["message"].as_str().unwrap().starts_with("internal error"), "{e}");
    }

    // Opt-in debug: raw text in error.debug only.
    let mut c = ix(&["sync", "--no-embed", "--file"]);
    let o = c.arg(dir.join("SECRET_missing.ost")).env("OUTLOOK_INDEX_DEBUG", "1").output().unwrap();
    let v: Json = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!((o.status.code(), v["error"]["kind"].as_str()), (Some(4), Some("io")));
    assert!(o.stdout.is_empty() && v["error"]["debug"].as_str().unwrap().contains("SECRET_missing.ost"), "{v}");
    assert!(!v["error"]["message"].as_str().unwrap().contains(MARK), "{v}");
    let mut c = ix(&["status"]);
    c.env("OUTLOOK_INDEX_TEST_PANIC", "run SECRET_PANIC").env("OUTLOOK_INDEX_DEBUG", "1");
    let v: Json = serde_json::from_slice(&c.output().unwrap().stderr).unwrap();
    assert!(v["error"]["debug"].as_str().unwrap().contains("SECRET_PANIC"), "{v}");

    std::fs::remove_dir_all(&dir).unwrap();
}
