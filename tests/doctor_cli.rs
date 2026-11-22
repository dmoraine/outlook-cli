//! `outlook-index doctor` errors: kind + fixed message on stderr, no path or file content.
#![cfg(feature = "index")]

use std::process::Command;

#[test]
fn doctor_errors_leak_nothing() {
    let dir = std::env::temp_dir().join(format!("SECRET_USER_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let junk = dir.join("SECRET_Alice.ost");
    std::fs::write(&junk, b"SECRET_BODY not a pst".repeat(100)).unwrap();
    let missing = dir.join("SECRET_missing.ost");
    for (file, kind) in [(&junk, "format"), (&missing, "io")] {
        let out = Command::new(env!("CARGO_BIN_EXE_outlook-index"))
            .args(["doctor", "--file"])
            .arg(file)
            .arg("--index")
            .arg(dir.join("SECRET_index.sqlite"))
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && out.stdout.is_empty(), "{err}");
        let v: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
        assert_eq!(v["error"]["kind"], kind, "{err}");
        assert!(!err.contains("SECRET"), "{err}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
