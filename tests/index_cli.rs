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
    std::fs::remove_dir_all(&dir).unwrap();
}
