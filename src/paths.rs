//! File paths: WSL path translation and the default OST location.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// Accept WSL-style paths when running as a Windows executable.
pub fn native_path(p: &str) -> PathBuf {
    if cfg!(windows) && p.starts_with('/') {
        let b = p.as_bytes();
        if p.len() >= 6 && p.starts_with("/mnt/") && b[5].is_ascii_alphabetic() && (p.len() == 6 || b[6] == b'/') {
            return PathBuf::from(format!(
                "{}:\\{}",
                (b[5] as char).to_ascii_uppercase(),
                p[6..].trim_start_matches('/').replace('/', "\\")
            ));
        }
        // Other absolute Linux paths: reach them through the WSL share of the
        // distribution we were started from (the current directory tells which).
        if let Ok(cwd) = std::env::current_dir() {
            let c = cwd.to_string_lossy().to_string();
            for prefix in [r"\\wsl.localhost\", r"\\wsl$\"] {
                if let Some(rest) = c.strip_prefix(prefix) {
                    let distro = rest.split('\\').next().unwrap_or("");
                    return PathBuf::from(format!("{prefix}{distro}{}", p.replace('/', "\\")));
                }
            }
        }
    }
    PathBuf::from(p)
}

pub fn default_file() -> Result<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(windows) {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(d).join("Microsoft").join("Outlook"));
        }
    } else if let Ok(users) = std::fs::read_dir("/mnt/c/Users") {
        for u in users.flatten() {
            dirs.push(u.path().join("AppData/Local/Microsoft/Outlook"));
        }
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ost")) {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                    best = Some((t, p));
                }
            }
        }
    }
    best.map(|b| b.1).ok_or_else(|| Error::Usage("no .ost file found; pass --file or set OUTLOOK_CLI_FILE".into()))
}
