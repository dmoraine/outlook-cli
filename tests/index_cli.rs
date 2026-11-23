//! `outlook-index` search/status CLI: usage errors, missing index, counts only.
#![cfg(feature = "index")]

use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// A child process killed and reaped when dropped (also on a test failure).
struct Reap(std::process::Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Output of `c`, which must end within `max`.
fn output_within(c: &mut Command, max: Duration) -> (Output, Duration) {
    let t = Instant::now();
    let mut c = Reap(c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap());
    while c.0.try_wait().unwrap().is_none() {
        assert!(t.elapsed() < max, "still running after {max:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    std::io::Read::read_to_end(&mut c.0.stdout.take().unwrap(), &mut stdout).unwrap();
    std::io::Read::read_to_end(&mut c.0.stderr.take().unwrap(), &mut stderr).unwrap();
    (Output { status: c.0.wait().unwrap(), stdout, stderr }, t.elapsed())
}

fn run(args: &[&str], index: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_outlook-index")).args(args).arg("--index").arg(index).output().unwrap()
}

fn err_kind(o: &Output) -> String {
    let v: serde_json::Value = serde_json::from_slice(&o.stderr).unwrap();
    v["error"]["kind"].as_str().unwrap().to_string()
}

#[test]
fn search_and_status_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-cli-{}", std::process::id()));
    let idx = dir.join("index.sqlite");
    let o = run(&["status"], &idx);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "not_found"));
    assert!(!idx.exists(), "status does not create the index");

    std::fs::create_dir_all(&dir).unwrap();
    outlook_cli::index::open(&idx).unwrap();
    let o = run(&["status"], &idx);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!((v["messages"].as_u64(), v["chunks"].as_u64()), (Some(0), Some(0)));

    let o = run(&["search", "\" * ( -"], &idx);
    assert_eq!(o.status.code(), Some(2), "no word");
    let o = run(&["search", "budget \"NEAR(", "--format", "jsonl", "--has-attachments", "--excerpt-max", "50"], &idx);
    assert!(o.status.success() && o.stdout.is_empty(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = run(&["search", "budget"], &idx);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["count"], 0);
    for bad in
        [&["search", "x", "--excerpt-max", "0"][..], &["search", "x", "--format", "text"], &["search", "x", "--jsonl"]]
    {
        assert_eq!(run(bad, &idx).status.code(), Some(2), "{bad:?}");
    }
    let o = run(&["status"], &idx);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["schema"]["version"].as_u64().is_some_and(|s| s >= 2), "{v}");
    assert_eq!(v["schema"]["upgrade_pending"], false, "{v}");
    assert!(v["last_sync"].get("revision").is_some() && v["last_sync"].get("at").is_some(), "{v}");
    let o = run(&["sync", "--max-time", "soon", "--file", "/nonexistent.ost"], &idx);
    assert_eq!(o.status.code(), Some(2));
    let o = run(&["sync", "--allow-mass-delete", "--file", "/nonexistent.ost"], &idx);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(4), "io"), "flag accepted");
    for ok in [&["sync", "--include-folders", "deleted,junk"][..], &["sync", "--include-folders", "none"]] {
        let o = run(&[ok, &["--file", "/nonexistent.ost"][..]].concat(), &idx);
        assert_eq!(o.status.code(), Some(4), "{ok:?} accepted");
    }
    assert_eq!(run(&["sync", "--include-folders", "trash"], &idx).status.code(), Some(2));
    // `none` with another value is ambiguous (N1), in both orders.
    for bad in ["none,deleted", "junk,none", "deleted,junk,none"] {
        let o = run(&["sync", "--include-folders", bad, "--file", "/nonexistent.ost"], &idx);
        assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(2), "usage"), "{bad}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Model commands without any download: missing files, usage errors, no curl.
#[test]
fn model_and_bench_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-model-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cmd = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_outlook-index"));
        c.args(args).env("XDG_DATA_HOME", &dir).env("LOCALAPPDATA", &dir).env_remove("OUTLOOK_INDEX_MODEL_DIR");
        c
    };
    let o = cmd(&["model", "list"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let names: Vec<&str> = v["models"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["e5-small", "e5-base"]);
    assert_eq!(v["models"][0]["ready"], false);
    assert!(v["models"][1]["dir"].as_str().unwrap().ends_with("multilingual-e5-base"), "{v}");

    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let o = cmd(&["bench-embed", "--seconds", "1", "--model-dir"]).arg(&empty).output().unwrap();
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "not_found"));
    for bad in [
        &["bench-embed", "--model", "e5-large"][..],
        &["bench-embed", "--seconds", "0"],
        &["bench-embed", "--threads", "0"],
        &["bench-embed", "--workers", "0"],
        &["embed", "--workers", "0"],
        &["bench-embed", "--batch", "0"],
        &["model", "fetch", "--model", "x"],
    ] {
        assert_eq!(cmd(bad).output().unwrap().status.code(), Some(2), "{bad:?}");
    }
    // No curl on PATH: a clear error, nothing left behind. (Windows would
    // still find curl.exe in System32, so Unix only.)
    #[cfg(unix)]
    {
        let o = cmd(&["model", "fetch", "--model-dir"]).arg(&empty).env("PATH", "").output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        let last: serde_json::Value = serde_json::from_str(err.lines().last().unwrap()).unwrap();
        assert_eq!((o.status.code(), last["error"]["kind"].as_str()), (Some(3), Some("not_found")), "{err}");
        assert!(err.lines().next().unwrap().contains("\"progress\""), "{err}");
        assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0, "no partial file");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `scope` set/show/clear and `embed` without model files. Counts only.
#[test]
fn scope_and_embed_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-scope-cli-{}", std::process::id()));
    let idx = dir.join("index.sqlite");
    let json = |o: &Output| -> serde_json::Value {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    };
    let o = run(&["scope"], &idx);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "not_found"), "show needs an index");
    let v =
        json(&run(&["scope", "--embed-since", "2026-01-01", "--embed-until", "2026-03-31", "-F", "SECRET_F"], &idx));
    let s = &v["scope"];
    assert_eq!((s["all"].as_bool(), s["folders"].as_u64()), (Some(false), Some(1)), "{v}");
    assert_eq!((s["since_input"].as_str(), s["until_input"].as_str()), (Some("2026-01-01"), Some("2026-03-31")));
    assert!(s["since"].as_str().is_some_and(|t| t.starts_with("202")), "{v}");
    assert!(!v.to_string().contains("SECRET"), "{v}");
    assert_eq!(v["chunks_in_scope"], 0);
    // Kept across runs; a partial change keeps the rest.
    let v = json(&run(&["scope", "--embed-until", "2026-06-30"], &idx));
    assert_eq!((v["scope"]["since_input"].as_str(), v["scope"]["folders"].as_u64()), (Some("2026-01-01"), Some(1)));
    let v = json(&run(&["status"], &idx));
    assert_eq!(v["embedding"]["scope"]["until_input"], "2026-06-30", "{v}");
    for bad in [
        &["scope", "--clear", "--embed-since", "3d"][..],
        &["scope", "--embed-since", "soon"],
        &["scope", "--embed-since", "2026-05-01", "--embed-until", "2026-04-01"],
    ] {
        assert_eq!(run(bad, &idx).status.code(), Some(2), "{bad:?}");
    }
    assert_eq!(json(&run(&["scope", "--clear"], &idx))["scope"]["all"], true);

    // Attachment chunk cap: default 30, persisted with the scope, 0 allowed.
    let v = json(&run(&["scope"], &idx));
    assert_eq!(
        (v["scope"]["attachment_chunks"].as_u64(), v["attachment_chunks"]["cap"].as_u64()),
        (Some(30), Some(30))
    );
    let v = json(&run(&["scope", "--attachment-chunks", "0"], &idx));
    assert_eq!((v["attachment_chunks"]["cap"].as_u64(), v["scope"]["all"].as_bool()), (Some(0), Some(true)), "{v}");
    let v = json(&run(&["scope", "--embed-since", "2026-01-01"], &idx));
    assert_eq!(v["scope"]["attachment_chunks"], 0, "kept: {v}");
    let v = json(&run(&["status"], &idx));
    assert_eq!(v["embedding"]["attachment_chunks"]["pending"], 0, "{v}");
    assert_eq!(json(&run(&["scope", "--clear"], &idx))["scope"]["attachment_chunks"], 30);
    for bad in [&["scope", "--attachment-chunks", "-1"][..], &["scope", "--clear", "--attachment-chunks", "5"]] {
        assert_eq!(run(bad, &idx).status.code(), Some(2), "{bad:?}");
    }

    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let o = run(&["embed", "--model-dir", empty.to_str().unwrap()], &idx);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "not_found"), "explicit when files are missing");
    let o = run(&["embed", "--workers", "2", "--threads", "1", "--model-dir", empty.to_str().unwrap()], &idx);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "not_found"), "from the worker processes");
    let help = run(&["--help"], &idx);
    assert!(!String::from_utf8_lossy(&help.stdout).contains("embed-worker"), "internal command hidden");
    for bad in [&["embed", "--batch", "0"][..], &["embed", "--priority", "high"], &["embed", "--max-time", "x"]] {
        assert_eq!(run(bad, &idx).status.code(), Some(2), "{bad:?}");
    }
    let o = run(&["sync", "--no-embed", "--model", "e5-base", "--file", "/nonexistent.ost"], &idx);
    assert_eq!(o.status.code(), Some(4), "flags accepted");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Search modes without a model: default, keyword fallback with a warning,
/// model mismatch. Fictitious data only.
#[test]
fn search_modes_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-modes-cli-{}", std::process::id()));
    let idx = dir.join("index.sqlite");
    std::fs::create_dir_all(&dir).unwrap();
    let cmd = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_outlook-index"))
            .args(args)
            .arg("--index")
            .arg(&idx)
            .env("XDG_DATA_HOME", &dir)
            .env("LOCALAPPDATA", &dir)
            .env_remove("OUTLOOK_INDEX_MODEL_DIR")
            .output()
            .unwrap()
    };
    let ok = |o: &Output| -> (serde_json::Value, String) {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        (serde_json::from_slice(&o.stdout).unwrap(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let c = outlook_cli::index::open(&idx).unwrap();
    let (v, err) = ok(&cmd(&["search", "budget"]));
    assert_eq!((v["mode"].as_str(), err.as_str()), (Some("keyword"), ""), "no vectors: keyword, no warning");
    for mode in ["semantic", "hybrid"] {
        let (v, err) = ok(&cmd(&["search", "budget", "--mode", mode]));
        let w: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
        assert_eq!((v["mode"].as_str(), w["warning"]["kind"].as_str()), (Some("keyword"), Some("keyword_fallback")));
    }

    // One fictitious message with an e5-small vector.
    let m = outlook_cli::index::model::get("e5-small").unwrap();
    c.execute_batch(
        "INSERT INTO folders VALUES (34, 'SECRET_FOLDER', 'SECRET_FOLDER');
         INSERT INTO messages(id, ident, received, has_attachments, subject) VALUES (1, 'mid:x', 0, 0, 'SECRET_SUBJECT');
         INSERT INTO locations VALUES (36, 34, 1, 1);
         INSERT INTO chunks(message, seq, text) VALUES (1, 0, 'SECRET_BODY budget');",
    )
    .unwrap();
    let blob: Vec<u8> = (0..m.dims).flat_map(|i| f32::from(u8::from(i == 0)).to_le_bytes()).collect();
    c.execute("INSERT INTO vectors VALUES (1, ?1, ?2, ?3)", rusqlite::params![m.name, m.revision, blob]).unwrap();
    for (k, v) in [("embed_model", m.name), ("embed_revision", m.revision)] {
        c.execute("INSERT OR REPLACE INTO meta VALUES (?1, ?2)", [k, v]).unwrap();
    }
    drop(c);

    let o = cmd(&["search", "budget", "--model", "e5-base"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(2), "usage"), "{err}");
    assert!(err.contains("e5-small") && !err.contains("SECRET") && !err.contains("budget"), "{err}");
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let o = cmd(&["search", "budget", "--mode", "semantic", "--model-dir", empty.to_str().unwrap()]);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "not_found"), "explicit mode needs the model");
    // Default mode without model files: keyword, with a warning.
    let (v, err) = ok(&cmd(&["search", "budget", "--model-dir", empty.to_str().unwrap()]));
    assert_eq!((v["mode"].as_str(), v["count"].as_u64()), (Some("keyword"), Some(1)), "{v}");
    assert!(err.contains("keyword_fallback") && !err.contains("SECRET"), "{err}");
    let (v, _) = ok(&cmd(&["search", "budget", "--mode", "keyword", "--model", "e5-base"]));
    assert_eq!(v["hits"][0]["ranks"], serde_json::json!({ "keyword": 1 }));
    for bad in [&["search", "budget", "--mode", "fuzzy"][..], &["search", "budget", "--model", "e5-large"]] {
        assert_eq!(cmd(bad).status.code(), Some(2), "{bad:?}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Model choice without model files: e5-base for a new index, else the
/// index's model; another model needs --reembed. Same rule for `model fetch`
/// and `bench-embed`.
#[test]
fn model_default_and_memory_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-default-cli-{}", std::process::id()));
    let (idx, empty) = (dir.join("index.sqlite"), dir.join("empty"));
    std::fs::create_dir_all(&empty).unwrap();
    let cmd = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_outlook-index"));
        c.args(args).arg("--index").arg(&idx).env("XDG_DATA_HOME", &dir).env("LOCALAPPDATA", &dir);
        c.env_remove("OUTLOOK_INDEX_MODEL_DIR").arg("--model-dir").arg(&empty);
        c
    };
    let fails = |args: &[&str], code: i32, model: &str| {
        let o = cmd(args).output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(o.status.code() == Some(code) && err.contains(model), "{args:?}: {err}");
    };
    // No index yet: e5-base.
    fails(&["bench-embed", "--seconds", "1"], 3, "e5-base");
    #[cfg(unix)]
    let fetched = |model: &str| {
        let o = cmd(&["model", "fetch"]).env("PATH", "").output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        let first: serde_json::Value = serde_json::from_str(err.lines().next().unwrap()).unwrap();
        assert_eq!(first["progress"]["model"], model, "{err}");
    };
    #[cfg(unix)]
    fetched("e5-base");
    assert!(!idx.exists(), "model commands do not create the index");
    let c = outlook_cli::index::open(&idx).unwrap();
    fails(&["embed"], 3, "e5-base");
    fails(&["sync", "--file", "/nonexistent.ost"], 4, "");
    // The index remembers its model.
    c.execute("INSERT OR REPLACE INTO meta VALUES ('embed_model', 'e5-small')", []).unwrap();
    fails(&["embed"], 3, "e5-small");
    fails(&["bench-embed", "--seconds", "1"], 3, "e5-small");
    #[cfg(unix)]
    fetched("e5-small");
    // Vectors of e5-small: another model needs --reembed.
    let m = outlook_cli::index::model::get("e5-small").unwrap();
    c.execute_batch(
        "INSERT INTO messages(id, ident, received, has_attachments) VALUES (1, 'mid:x', 0, 0);
         INSERT INTO chunks(message, seq, text) VALUES (1, 0, 'x');",
    )
    .unwrap();
    let blob = vec![0u8; m.dims * 4];
    c.execute("INSERT INTO vectors VALUES (1, ?1, ?2, ?3)", rusqlite::params![m.name, m.revision, blob]).unwrap();
    fails(&["embed", "--model", "e5-base"], 2, "--reembed");
    fails(&["embed", "--model", "e5-base", "--reembed"], 3, "e5-base");
    fails(&["embed"], 3, "e5-small");
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Attachment scope and `--no-attachments` on the command line. Counts only.
#[test]
fn attachments_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-att-cli-{}", std::process::id()));
    let idx = dir.join("index.sqlite");
    let json = |o: &Output| -> serde_json::Value {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    };
    let v = json(&run(&["scope", "--attachment-types", "pdf,docx", "--attachment-max-mb", "5"], &idx));
    let a = &v["attachments"];
    assert_eq!(a["scope"], serde_json::json!({ "types": ["pdf", "docx"], "max_bytes": 5 << 20 }), "{v}");
    assert_eq!((a["distinct"].as_u64(), a["out_of_scope"].as_u64()), (Some(0), Some(0)), "{v}");
    let v = json(&run(&["scope", "--embed-since", "2026-01-01"], &idx));
    assert_eq!(v["attachments"]["scope"]["max_bytes"], 5 << 20, "kept: {v}");
    let v = json(&run(&["status"], &idx));
    assert_eq!(v["attachments"]["scope"]["types"], serde_json::json!(["pdf", "docx"]), "{v}");
    let v = json(&run(&["scope", "--clear"], &idx));
    assert_eq!(v["attachments"]["scope"]["types"].as_array().map(Vec::len), Some(7), "{v}");
    for bad in [
        &["scope", "--attachment-max-mb", "0"][..],
        &["scope", "--attachment-max-mb", "101"],
        &["scope", "--attachment-types", "zip"],
        &["scope", "--clear", "--attachment-types", "pdf"],
    ] {
        assert_eq!(run(bad, &idx).status.code(), Some(2), "{bad:?}");
    }
    let o = run(&["sync", "--no-attachments", "--no-embed", "--file", "/nonexistent.ost"], &idx);
    assert_eq!(o.status.code(), Some(4), "flag accepted");
    // S10: the extractor memory limit override, in MB.
    let o = run(&["sync", "--extract-memory-mb", "300", "--no-embed", "--file", "/nonexistent.ost"], &idx);
    assert_eq!(o.status.code(), Some(4), "flag accepted: {}", String::from_utf8_lossy(&o.stderr));
    for bad in ["0", "127", "lots"] {
        assert_eq!(run(&["sync", "--extract-memory-mb", bad], &idx).status.code(), Some(2), "{bad}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `status.running`, `status --watch` and the progress flags. Fictitious records only.
#[test]
fn progress_and_watch_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-watch-cli-{}", std::process::id()));
    let idx = dir.join("index.sqlite");
    std::fs::create_dir_all(&dir).unwrap();
    let c = outlook_cli::index::open(&idx).unwrap();
    let json = |o: &Output| -> serde_json::Value {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    };
    assert_eq!(json(&run(&["status"], &idx))["running"], serde_json::Value::Null);
    let o = run(&["status", "--watch", "1"], &idx);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "{\"running\":null}\n", "no run: one line");

    let record = |phase: &str, ago_s: i64| {
        let at = chrono::Utc::now() - chrono::Duration::seconds(ago_s);
        let r = serde_json::json!({
            "phase": phase, "done": 5, "total": 10, "percent": 50.0, "rate": 1.5, "eta_s": 4, "elapsed_s": 60,
            "workers": 2, "model": "e5-base", "pid": 4242, "started_at": "2026-11-22T10:00:00Z",
            "updated_at": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), "interval_s": 1,
        });
        c.execute("INSERT OR REPLACE INTO meta VALUES ('sync_progress', ?1)", [r.to_string()]).unwrap();
    };
    // A crashed run: stale, watch prints it once and stops.
    record("embed_bodies", 600);
    let v = json(&run(&["status"], &idx));
    assert_eq!((v["running"]["phase"].as_str(), v["running"]["stale"].as_bool()), (Some("embed_bodies"), Some(true)));
    let o = run(&["status", "--watch", "1", "--pretty"], &idx);
    let lines: Vec<&str> = std::str::from_utf8(&o.stdout).unwrap().lines().collect();
    assert_eq!(lines.len(), 1, "jsonl even with --pretty: {lines:?}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["running"]["stale"], true);

    // A live run (lock held): one line per tick until the record says done.
    let lock = outlook_cli::index::sync::lock(&idx).unwrap();
    record("index", 0);
    // Handshake (S8): read its lines until two live ticks, then mark done and
    // read until it stops; one overall deadline, the child killed on failure.
    let mut child = Reap(
        Command::new(env!("CARGO_BIN_EXE_outlook-index"))
            .args(["status", "--watch", "1", "--index"])
            .arg(&idx)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let out = std::io::BufReader::new(child.0.stdout.take().unwrap());
    let reader = std::thread::spawn(move || {
        for l in std::io::BufRead::lines(out) {
            let _ = tx.send(serde_json::from_str::<serde_json::Value>(&l.unwrap()).unwrap());
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut lines = Vec::new();
    let mut until = |phase: &str, n: usize| {
        while lines.iter().filter(|l: &&serde_json::Value| l["running"]["phase"] == phase).count() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            lines.push(rx.recv_timeout(left).unwrap_or_else(|e| panic!("waiting for {phase}: {e:?} after {lines:?}")));
        }
    };
    until("index", 2);
    let v = json(&run(&["status"], &idx));
    assert_eq!((v["running"]["phase"].as_str(), v["running"]["stale"].as_bool()), (Some("index"), Some(false)), "{v}");
    record("done", 0);
    drop(lock);
    until("done", 1);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "watch still running");
        std::thread::sleep(Duration::from_millis(50));
    }
    reader.join().unwrap();
    assert!(lines.len() >= 3, "{lines:?}");
    assert_eq!(lines[0]["running"]["phase"], "index");
    assert_eq!(lines.last().unwrap()["running"]["phase"], "done");
    assert_eq!(json(&run(&["status"], &idx))["running"], serde_json::Value::Null, "finished");

    for ok in [&["sync", "--progress", "jsonl", "--progress-interval", "5"][..], &["sync", "--progress", "none"]] {
        let o = run(&[ok, &["--file", "/nonexistent.ost"][..]].concat(), &idx);
        assert_eq!(o.status.code(), Some(4), "{ok:?} accepted");
    }
    for bad in [
        &["sync", "--progress", "fancy"][..],
        &["sync", "--progress-interval", "0"],
        &["embed", "--progress", "x"],
        &["status", "--watch", "0"],
    ] {
        assert_eq!(run(bad, &idx).status.code(), Some(2), "{bad:?}");
    }
    let o = run(&["embed", "--progress", "bar", "--model-dir", dir.join("none").to_str().unwrap()], &idx);
    assert_eq!(o.status.code(), Some(3), "accepted");
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A schema-4 index (before attachments) with one fictitious message, as an
/// older build left it, checkpointed into the main file.
fn old_index(dir: &std::path::Path) -> std::path::PathBuf {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let idx = dir.join("index.sqlite");
    let c = outlook_cli::index::open(&idx).unwrap();
    c.execute_batch(
        "DROP TABLE attachment_vectors; DROP TABLE attachment_fts; DROP TABLE attachment_chunks;
         DROP TABLE message_attachments; DROP TABLE attachment_contents;
         ALTER TABLE messages DROP COLUMN attachments_done;
         ALTER TABLE messages DROP COLUMN signature;
         DELETE FROM schema_version WHERE version > 4;
         INSERT INTO folders VALUES (34, 'Inbox', 'Inbox');
         INSERT INTO messages(id, ident, received, has_attachments, subject) VALUES (1, 'mid:x', 0, 0, 'Budget');
         INSERT INTO locations VALUES (36, 34, 1, 1);
         INSERT INTO chunks(message, seq, text) VALUES (1, 0, 'budget review');",
    )
    .unwrap();
    drop(c);
    idx
}

/// Schema version stored in the main file, read without migrating.
fn file_schema(idx: &std::path::Path) -> i64 {
    let c = rusqlite::Connection::open_with_flags(idx, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    c.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0)).unwrap()
}

fn writer(idx: &std::path::Path) -> Output {
    run(&["scope", "--attachment-max-mb", "20"], idx)
}

/// S2: read-only commands on an older schema neither migrate nor write;
/// status reports the pending upgrade, search asks for a sync; a writer
/// waits for the sync lock before migrating; concurrent readers and writers
/// on an old index migrate once.
#[test]
fn old_schema_readers_and_writers_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-old-cli-{}", std::process::id()));
    let idx = old_index(&dir);
    let bytes = std::fs::read(&idx).unwrap();
    let o = run(&["status"], &idx);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let s = &v["schema"];
    assert_eq!((s["version"].as_u64(), s["upgrade_pending"].as_bool()), (Some(4), Some(true)), "{v}");
    assert!(s["expected"].as_u64().is_some_and(|e| e > 4), "{v}");
    assert_eq!(v["messages"], 1, "{v}");
    let o = run(&["search", "budget"], &idx);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "upgrade_pending"), "{o:?}");
    let o = run(&["scope"], &idx);
    assert_eq!(err_kind(&o), "upgrade_pending", "{o:?}");
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let o = run(&["bench-embed", "--seconds", "1", "--model-dir", empty.to_str().unwrap()], &idx);
    assert_eq!(o.status.code(), Some(3), "{o:?}");
    let o = run(&["status", "--watch", "1"], &idx);
    assert!(o.status.success(), "{o:?}");
    assert_eq!(
        (file_schema(&idx), std::fs::read(&idx).unwrap() == bytes),
        (4, true),
        "read-only commands write nothing"
    );

    // A writer while another process holds the sync lock: refused, nothing migrated.
    let lock = outlook_cli::index::sync::lock(&idx).unwrap();
    let t = std::time::Instant::now();
    let o = writer(&idx);
    let waited = t.elapsed();
    assert!(
        waited >= std::time::Duration::from_millis(2500) && waited < std::time::Duration::from_secs(20),
        "{waited:?}"
    );
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(2), "usage"), "{o:?}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("another sync is running"), "{o:?}");
    assert_eq!((file_schema(&idx), std::fs::read(&idx).unwrap() == bytes), (4, true), "no migration without the lock");
    drop(lock);
    let o = writer(&idx);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&run(&["status"], &idx).stdout).unwrap();
    assert_eq!(v["schema"]["upgrade_pending"], false, "{v}");
    assert!(run(&["search", "budget"], &idx).status.success());

    // Old index, several readers and writers at once.
    let idx = old_index(&dir);
    let p = idx.as_path();
    let outs: Vec<(bool, Output)> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8)
            .map(|i| s.spawn(move || if i % 2 == 0 { (true, writer(p)) } else { (false, run(&["status"], p)) }))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (w, o) in &outs {
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(o.status.success(), "writer={w}: {err}");
        assert!(!err.contains("duplicate"), "{err}");
    }
    let c = rusqlite::Connection::open(&idx).unwrap();
    let versions: Vec<i64> = c
        .prepare("SELECT version FROM schema_version ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(versions, (1..=versions.len() as i64).collect::<Vec<_>>());
    assert!(versions.len() > 4);

    // A newer schema: readers and writers refuse it, nothing written.
    c.execute("INSERT INTO schema_version(version) VALUES (99)", []).unwrap();
    drop(c);
    let bytes = std::fs::read(&idx).unwrap();
    for args in [&["status"][..], &["search", "budget"], &["scope"], &["scope", "--attachment-max-mb", "20"]] {
        let o = run(args, &idx);
        assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(5), "format"), "{args:?}: {o:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("newer than this tool"), "{o:?}");
    }
    assert_eq!(std::fs::read(&idx).unwrap(), bytes);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// S9: an observer probing the lock (status) holds it shared for a moment;
/// a writer starting meanwhile waits a little instead of being refused.
#[test]
fn observer_probe_does_not_refuse_writer_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-probe-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let idx = dir.join("index.sqlite");
    drop(outlook_cli::index::sync::lock(&idx).unwrap());
    outlook_cli::index::open(&idx).unwrap();
    let t = std::time::Instant::now();
    assert!(!outlook_cli::index::sync::held(&idx));
    assert!(t.elapsed() < std::time::Duration::from_millis(100), "the probe stays short");
    let (tx, rx) = std::sync::mpsc::channel();
    let lock = dir.join("index.sqlite.lock");
    let probe = std::thread::spawn(move || {
        let f = std::fs::File::open(&lock).unwrap();
        f.lock_shared().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
    });
    rx.recv().unwrap();
    let o = writer(&idx);
    probe.join().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Processes still alive whose environment holds `token` (Linux only).
fn alive_with(token: &str) -> usize {
    if !cfg!(target_os = "linux") {
        return 0;
    }
    let Ok(d) = std::fs::read_dir("/proc") else { return 0 };
    d.flatten()
        .filter(|e| {
            std::fs::read(e.path().join("environ")).is_ok_and(|b| b.windows(token.len()).any(|w| w == token.as_bytes()))
        })
        .count()
}

/// S3: embedding workers that never load, hang, send half a reply or exit
/// end the run within --worker-timeout: killed and reaped, a resumable io
/// error, the lock free, the vectors already stored kept. Driven by the
/// worker's test hook (OUTLOOK_INDEX_TEST_WORKER, fake model, no files).
#[test]
fn embedding_worker_timeouts_cli() {
    let dir = std::env::temp_dir().join(format!("outlook-index-wtimeout-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let idx = dir.join("index.sqlite");
    let c = outlook_cli::index::open(&idx).unwrap();
    c.execute_batch(
        "INSERT INTO folders VALUES (34, 'F', 'F');
         INSERT INTO messages(id, ident, received, has_attachments, subject) VALUES (1, 'mid:x', 0, 0, 'S');
         INSERT INTO locations VALUES (36, 34, 1, 1);",
    )
    .unwrap();
    for i in 0..4 {
        c.execute("INSERT INTO chunks(message, seq, text) VALUES (1, ?1, 'fictitious text')", [i]).unwrap();
    }
    let embed = |hook: &str| {
        let token = format!("wt-{}-{}", std::process::id(), hook.replace(' ', "-"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_outlook-index"));
        cmd.args(["embed", "--model", "e5-small", "--workers", "2", "--threads", "1", "--batch", "1"])
            .args(["--worker-timeout", "1s", "--progress", "none", "--model-dir"])
            .arg(&dir)
            .arg("--index")
            .arg(&idx)
            .env("OUTLOOK_INDEX_TEST_WORKER", format!("{hook} {token}"));
        let (o, t) = output_within(&mut cmd, Duration::from_secs(30));
        assert!(t < Duration::from_secs(15), "{hook}: {t:?}");
        assert_eq!(alive_with(&token), 0, "{hook}: no process left");
        o
    };
    let vectors = || -> (i64, i64) {
        c.query_row("SELECT count(*), COALESCE(SUM(length(vec) = 384 * 4), 0) FROM vectors", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    };
    for (hook, msg) in
        [("never_ready 0", "timed out"), ("hang 0", "timed out"), ("partial 0", "timed out"), ("exit 0", "exited")]
    {
        let o = embed(hook);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(4), "io"), "{hook}: {err}");
        assert!(err.contains(msg), "{hook}: {err}");
        drop(outlook_cli::index::sync::lock(&idx).expect("lock released"));
        assert_eq!(vectors(), (0, 0), "{hook}");
    }
    // One good batch per worker, then hung: what was stored stays, valid.
    let o = embed("hang 1");
    assert_eq!(o.status.code(), Some(4), "{}", String::from_utf8_lossy(&o.stderr));
    let (n, valid) = vectors();
    assert!((1..4).contains(&n) && valid == n, "{n} {valid}");
    // The next run resumes with the rest.
    let o = embed("hang 9");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!((v["embedded"].as_i64(), v["complete"].as_bool()), (Some(4 - n), Some(true)), "{v}");
    assert_eq!(vectors(), (4, 4));
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}
