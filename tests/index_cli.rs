//! `outlook-index` search/status CLI: usage errors, missing index, counts only.
#![cfg(feature = "index")]

use std::process::{Command, Output};

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
    assert!(v["schema"].as_u64().is_some_and(|s| s >= 2), "{v}");
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
    let child = Command::new(env!("CARGO_BIN_EXE_outlook-index"))
        .args(["status", "--watch", "1", "--index"])
        .arg(&idx)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let v = json(&run(&["status"], &idx));
    assert_eq!((v["running"]["phase"].as_str(), v["running"]["stale"].as_bool()), (Some("index"), Some(false)), "{v}");
    record("done", 0);
    drop(lock);
    let o = child.wait_with_output().unwrap();
    let lines: Vec<serde_json::Value> =
        std::str::from_utf8(&o.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
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
