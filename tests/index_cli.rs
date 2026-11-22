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

    for mode in ["semantic", "hybrid"] {
        let o = run(&["search", "x", "--mode", mode], &idx);
        assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(2), "usage"), "{mode}");
    }
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

    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let o = run(&["embed", "--model-dir", empty.to_str().unwrap()], &idx);
    assert_eq!((o.status.code(), err_kind(&o).as_str()), (Some(3), "not_found"), "explicit when files are missing");
    for bad in [&["embed", "--batch", "0"][..], &["embed", "--priority", "high"], &["embed", "--max-time", "x"]] {
        assert_eq!(run(bad, &idx).status.code(), Some(2), "{bad:?}");
    }
    let o = run(&["sync", "--no-embed", "--model", "e5-base", "--file", "/nonexistent.ost"], &idx);
    assert_eq!(o.status.code(), Some(4), "flags accepted");
    std::fs::remove_dir_all(&dir).unwrap();
}
