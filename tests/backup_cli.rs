//! F5: `outlook-index backup` (S5) and owner-only index files (S6).
#![cfg(feature = "index")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value as Json;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_outlook-index")
}

fn run(args: &[&str], index: &Path) -> Output {
    Command::new(bin()).args(args).arg("--index").arg(index).output().unwrap()
}

fn json(o: &Output) -> Json {
    assert!(o.status.success(), "{:?} {}", o.status.code(), String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn backup_to(idx: &Path, to: &Path) -> Json {
    json(&Command::new(bin()).arg("backup").arg("--to").arg(to).arg("--index").arg(idx).output().unwrap())
}

fn fresh(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("outlook-index-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn side(p: &Path, s: &str) -> PathBuf {
    let mut o = p.as_os_str().to_owned();
    o.push(s);
    o.into()
}

/// One fictitious message with one chunk and its vector, in one transaction.
fn insert(c: &mut rusqlite::Connection, id: i64) {
    let tx = c.transaction().unwrap();
    tx.execute("INSERT INTO messages(id, ident, received, has_attachments) VALUES (?1, 'mid:' || ?1, ?1, 0)", [id])
        .unwrap();
    tx.execute("INSERT INTO chunks(id, message, seq, text) VALUES (?1, ?1, 0, 'fictitious text ' || ?1)", [id])
        .unwrap();
    tx.execute("INSERT INTO vectors VALUES (?1, 'e5-small', 'r', x'00000000')", [id]).unwrap();
    tx.commit().unwrap();
}

fn ids(p: &Path) -> Vec<i64> {
    let c = rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    c.prepare("SELECT id FROM messages ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// The documented restore: all users stopped, sidecars removed, backup copied.
fn restore(backup: &Path, idx: &Path) {
    for s in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(side(idx, s));
    }
    std::fs::copy(backup, idx).unwrap();
}

/// A backup taken while a writer commits is one consistent snapshot: every
/// message with its chunk and vector, integrity ok, summary = file content.
#[test]
fn backup_during_concurrent_writer() {
    let dir = fresh("backup-writer");
    let idx = dir.join("index.sqlite");
    let mut c = outlook_cli::index::open(&idx).unwrap();
    for i in 1..=200 {
        insert(&mut c, i);
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    let w = std::thread::spawn(move || {
        let mut i = 201;
        while !s.load(std::sync::atomic::Ordering::Relaxed) {
            insert(&mut c, i);
            i += 1;
        }
        i - 1
    });
    let mut last = 0;
    for n in 0..4 {
        let to = dir.join(format!("b{n}.sqlite"));
        let v = backup_to(&idx, &to);
        assert!(side(&idx, "-wal").exists(), "the writer keeps a WAL");
        assert_eq!(v["integrity"], "ok", "{v}");
        assert_eq!(v["destination"], to.to_str().unwrap(), "{v}");
        assert_eq!(v["bytes"].as_u64(), Some(std::fs::metadata(&to).unwrap().len()), "{v}");
        assert_eq!(v["schema_version"], json(&run(&["status"], &idx))["schema"]["expected"], "{v}");
        let b = rusqlite::Connection::open_with_flags(&to, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let count = |t: &str| b.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap();
        let m = count("messages");
        assert!(m >= 200 && m >= last, "{m}");
        assert_eq!((count("chunks"), count("vectors")), (m, m), "snapshot torn");
        assert_eq!((v["counts"]["messages"].as_i64(), v["counts"]["vectors"].as_i64()), (Some(m), Some(m)), "{v}");
        assert_eq!(ids(&to), (1..=m).collect::<Vec<_>>());
        assert!(!side(&to, "-wal").exists() && !side(&to, "-shm").exists(), "self-contained backup");
        last = m;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let total = w.join().unwrap();
    assert!(total >= last);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Repeated backups while the source WAL appears and disappears; restoring
/// each gives exactly its content (an obsolete WAL is never replayed).
#[test]
fn repeated_backups_and_restore() {
    let dir = fresh("backup-restore");
    let idx = dir.join("index.sqlite");
    let mut c = outlook_cli::index::open(&idx).unwrap();
    (1..=3).for_each(|i| insert(&mut c, i));
    assert!(side(&idx, "-wal").exists());
    let b1 = backup_to(&idx, &dir.join("b1.sqlite"));
    (4..=5).for_each(|i| insert(&mut c, i));
    drop(c);
    assert!(!side(&idx, "-wal").exists(), "last close checkpoints and removes the WAL");
    let b2 = backup_to(&idx, &dir.join("b2.sqlite"));
    let mut c = outlook_cli::index::open(&idx).unwrap();
    (6..=9).for_each(|i| insert(&mut c, i));
    drop(c);
    assert_eq!((b1["counts"]["messages"].as_i64(), b2["counts"]["messages"].as_i64()), (Some(3), Some(5)));

    restore(&dir.join("b1.sqlite"), &idx);
    assert_eq!(ids(&idx), [1, 2, 3]);
    let v = json(&run(&["status"], &idx));
    assert_eq!((v["messages"].as_i64(), v["chunks"].as_i64()), (Some(3), Some(3)), "{v}");
    let mut c = outlook_cli::index::open(&idx).unwrap();
    insert(&mut c, 4);
    drop(c);
    restore(&dir.join("b2.sqlite"), &idx);
    assert_eq!(ids(&idx), [1, 2, 3, 4, 5]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An existing destination (even empty) is refused and left as it is; the
/// default destination is a new file in `backups/` next to the index.
#[test]
fn backup_destinations() {
    let dir = fresh("backup-dest");
    let idx = dir.join("index.sqlite");
    let o = run(&["backup"], &idx);
    assert_eq!(o.status.code(), Some(3), "no index: {}", String::from_utf8_lossy(&o.stderr));
    assert!(!idx.exists() && !dir.join("backups").exists(), "nothing created");

    let mut c = outlook_cli::index::open(&idx).unwrap();
    insert(&mut c, 1);
    for (name, content) in [("full.sqlite", &b"keep me"[..]), ("empty.sqlite", b"")] {
        let to = dir.join(name);
        std::fs::write(&to, content).unwrap();
        let o = Command::new(bin()).arg("backup").arg("--to").arg(&to).arg("--index").arg(&idx).output().unwrap();
        let e: Json = serde_json::from_slice(&o.stderr).unwrap();
        assert_eq!((o.status.code(), e["error"]["kind"].as_str()), (Some(2), Some("usage")), "{e}");
        assert!(!e.to_string().contains(name), "no path in the error: {e}");
        assert_eq!(std::fs::read(&to).unwrap(), content, "destination untouched");
    }

    let v = json(&run(&["backup"], &idx));
    let to = PathBuf::from(v["destination"].as_str().unwrap());
    assert_eq!(to.parent(), Some(dir.join("backups").as_path()), "{v}");
    let name = to.file_name().unwrap().to_str().unwrap();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let stamp = name.strip_prefix("index-").and_then(|s| s.strip_suffix(".sqlite")).unwrap_or_default();
    assert!(stamp.len() == 15 && digits(&stamp[..8]) && &stamp[8..9] == "-" && digits(&stamp[9..]), "{name}");
    assert_eq!(ids(&to), [1]);
    #[cfg(unix)]
    {
        assert_eq!(mode(&to), 0o600, "backup holds mail content");
        assert_eq!(mode(&dir.join("backups")), 0o700);
    }
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn chmod(p: &Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

/// S6: under umask 022 a new index directory is 0700 and the database, its
/// lock and SQLite sidecars 0600; an existing parent keeps its mode.
#[cfg(unix)]
#[test]
fn new_index_is_private_under_umask_022() {
    let dir = fresh("private");
    let shared = dir.join("shared");
    std::fs::create_dir(&shared).unwrap();
    chmod(&shared, 0o755);
    let idx = shared.join("sub").join("index.sqlite");
    let o = Command::new("sh")
        .args(["-c", "umask 022; exec \"$0\" \"$@\""])
        .arg(bin())
        .args(["sync", "--no-embed", "--file", "/nonexistent.ost", "--index"])
        .arg(&idx)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(4), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(mode(&shared), 0o755, "existing parent untouched");
    assert_eq!(mode(&shared.join("sub")), 0o700);
    assert_eq!(mode(&idx), 0o600);
    assert_eq!(mode(&side(&idx, ".lock")), 0o600);
    let c = outlook_cli::index::open(&idx).unwrap();
    for s in ["-wal", "-shm"] {
        assert_eq!(mode(&side(&idx, s)), 0o600, "{s}");
    }
    drop(c);
    let v = json(&run(&["status"], &idx));
    assert!(v.get("permissions_warning").is_some_and(Json::is_null), "{v}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// S6: an existing location open to group/other gets a path-free
/// `permissions_warning` in status; no command changes its modes.
#[cfg(unix)]
#[test]
fn permissive_location_warns() {
    let dir = fresh("permissive");
    let loc = dir.join("SECRET_USER");
    std::fs::create_dir(&loc).unwrap();
    let idx = loc.join("index.sqlite");
    drop(outlook_cli::index::open(&idx).unwrap());
    chmod(&loc, 0o755);
    chmod(&idx, 0o644);
    let v = json(&run(&["status"], &idx));
    let w = v["permissions_warning"].as_str().unwrap_or_else(|| panic!("{v}"));
    assert!(!w.contains("SECRET") && !w.contains('/'), "{w}");
    let o = run(&["sync", "--no-embed", "--file", "/nonexistent.ost"], &idx);
    assert_eq!(o.status.code(), Some(4));
    assert_eq!((mode(&loc), mode(&idx)), (0o755, 0o644), "never chmod an existing location");

    chmod(&idx, 0o600);
    assert!(json(&run(&["status"], &idx))["permissions_warning"].is_string(), "directory still open");
    chmod(&loc, 0o700);
    assert!(json(&run(&["status"], &idx))["permissions_warning"].is_null());
    std::fs::remove_dir_all(&dir).unwrap();
}
