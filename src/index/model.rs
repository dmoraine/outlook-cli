//! Embedding model files: pinned revisions, SHA-256 checks, model
//! directories and the opt-in download (`outlook-index model fetch`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub struct Model {
    /// CLI name.
    pub name: &'static str,
    /// Hugging Face repository.
    pub repo: &'static str,
    /// Pinned commit of the repository.
    pub revision: &'static str,
    pub dims: usize,
    pub files: [ModelFile; 3],
}

/// Pinned from the Hugging Face API (LFS sha256; config.json: git blob id
/// checked, then sha256 of the downloaded file).
pub const MODELS: [Model; 2] = [
    Model {
        name: "e5-small",
        repo: "intfloat/multilingual-e5-small",
        revision: "614241f622f53c4eeff9890bdc4f31cfecc418b3",
        dims: 384,
        files: [
            ModelFile {
                name: "config.json",
                size: 655,
                sha256: "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959",
            },
            ModelFile {
                name: "tokenizer.json",
                size: 17_082_730,
                sha256: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
            },
            ModelFile {
                name: "model.safetensors",
                size: 470_641_600,
                sha256: "1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477",
            },
        ],
    },
    Model {
        name: "e5-base",
        repo: "intfloat/multilingual-e5-base",
        revision: "d128750597153bb5987e10b1c3493a34e5a4502a",
        dims: 768,
        files: [
            ModelFile {
                name: "config.json",
                size: 694,
                sha256: "9dab198f24c8c0879e481cf7822005d5ecbceedbacb390ffafa594e28d31bac4",
            },
            ModelFile {
                name: "tokenizer.json",
                size: 17_082_660,
                sha256: "62c24cdc13d4c9952d63718d6c9fa4c287974249e16b7ade6d5a85e7bbb75626",
            },
            ModelFile {
                name: "model.safetensors",
                size: 1_112_201_288,
                sha256: "a18a44fad1d0b46ded15928144138cff1135d5cc8233bdd90be5f18822de09a7",
            },
        ],
    },
];

pub const NAMES: [&str; 2] = [MODELS[0].name, MODELS[1].name];

/// Model of a new index.
pub const DEFAULT: &str = "e5-base";

/// `requested`, else the index's `stored` model, else DEFAULT. A model other
/// than the stored one is then refused by `vectors::check_model` unless reembed.
pub fn choose(requested: Option<&str>, stored: Option<&str>) -> Result<&'static Model> {
    get(requested.or(stored).unwrap_or(DEFAULT))
}

pub fn get(name: &str) -> Result<&'static Model> {
    MODELS.iter().find(|m| m.name == name).ok_or_else(|| Error::Usage(format!("unknown model '{name}'")))
}

/// <data dir>/models/<repository name>.
pub fn default_dir(m: &Model) -> Result<PathBuf> {
    let d = super::data_dir().ok_or_else(|| {
        Error::Usage("no data directory found; pass --model-dir or set OUTLOOK_INDEX_MODEL_DIR".into())
    })?;
    Ok(d.join("models").join(m.repo.rsplit('/').next().unwrap_or(m.name)))
}

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut r = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        match r.read(&mut buf)? {
            0 => break,
            n => h.update(&buf[..n]),
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn missing(m: &Model, name: &str) -> Error {
    Error::NotFound(format!(
        "model file {name} of {}; run `outlook-index model fetch --model {}` or pass --model-dir",
        m.name, m.name
    ))
}

/// Check one file against its pinned size and hash.
fn check(m: &Model, f: &ModelFile, p: &Path) -> Result<()> {
    let size = std::fs::metadata(p).map_err(|_| missing(m, f.name))?.len();
    let ok = size == f.size && sha256_file(p).map_err(|e| Error::Io(format!("{}: {e}", f.name)))? == f.sha256;
    if ok {
        Ok(())
    } else {
        Err(Error::Format(format!("model file {} does not match {} revision {}", f.name, m.repo, m.revision)))
    }
}

/// Check every file of `m` in `dir` (size and SHA-256).
pub fn verify(m: &Model, dir: &Path) -> Result<()> {
    m.files.iter().try_for_each(|f| check(m, f, &dir.join(f.name)))
}

/// Presence of the files in `dir` (sizes only, no hashing).
pub fn list(dir: impl Fn(&Model) -> Result<PathBuf>) -> Result<Json> {
    let mut out = Vec::new();
    for m in &MODELS {
        let d = dir(m)?;
        let files: Vec<Json> = m
            .files
            .iter()
            .map(|f| {
                let size = std::fs::metadata(d.join(f.name)).ok().map(|x| x.len());
                json!({ "name": f.name, "size": f.size, "present": size == Some(f.size) })
            })
            .collect();
        let ready = files.iter().all(|f| f["present"] == true);
        out.push(json!({
            "name": m.name, "repo": m.repo, "revision": m.revision, "dims": m.dims,
            "dir": d.to_string_lossy(), "files": files, "ready": ready,
        }));
    }
    Ok(json!({ "models": out }))
}

/// Verify a finished download and move it into place; a bad `.part` is deleted.
fn install_part(m: &Model, f: &ModelFile, part: &Path, dest: &Path) -> Result<()> {
    if let Err(e) = check(m, f, part) {
        let _ = std::fs::remove_file(part);
        return Err(e);
    }
    std::fs::rename(part, dest).map_err(|e| Error::Io(format!("{}: {e}", f.name)))
}

/// Download the missing or mismatched files of `m` into `dir` over HTTPS from
/// huggingface.co at the pinned revision, with `curl`. Files go to `<name>.part`
/// (resumed if shorter than expected) and are renamed only after the SHA-256 check.
pub fn fetch(m: &Model, dir: &Path, mut progress: impl FnMut(&ModelFile)) -> Result<Json> {
    std::fs::create_dir_all(dir).map_err(|e| Error::Io(format!("cannot create the model directory: {e}")))?;
    let mut files = Vec::new();
    for f in &m.files {
        let dest = dir.join(f.name);
        let have = check(m, f, &dest).is_ok();
        if !have {
            progress(f);
            let part = dir.join(format!("{}.part", f.name));
            let resume = std::fs::metadata(&part).is_ok_and(|x| x.len() < f.size);
            if !resume {
                let _ = std::fs::remove_file(&part);
            }
            let url = format!("https://huggingface.co/{}/resolve/{}/{}", m.repo, m.revision, f.name);
            curl(&url, &part, resume)?;
            install_part(m, f, &part, &dest)?;
        }
        files.push(json!({ "name": f.name, "size": f.size, "sha256": f.sha256, "downloaded": !have }));
    }
    Ok(json!({ "name": m.name, "repo": m.repo, "revision": m.revision, "dir": dir.to_string_lossy(), "files": files }))
}

fn curl(url: &str, part: &Path, resume: bool) -> Result<()> {
    let mut c = Command::new("curl");
    // HTTPS only, redirects included (huggingface.co sends large files to its CDN).
    c.args(["--proto", "=https", "--proto-redir", "=https", "--tlsv1.2", "-fsSL", "--retry", "3"]);
    c.args(["--connect-timeout", "30", "-o"]).arg(part);
    if resume {
        c.args(["-C", "-"]);
    }
    let o = c.arg(url).output().map_err(|_| {
        Error::NotFound("curl not found; download the model files manually and pass --model-dir".into())
    })?;
    if o.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&o.stderr);
        Err(Error::Io(format!("download failed (curl {}): {}", o.status.code().unwrap_or(-1), msg.trim())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("outlook-index-model-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pinned_models() {
        for m in &MODELS {
            assert_eq!(m.revision.len(), 40, "{}", m.name);
            assert!(m.repo.starts_with("intfloat/multilingual-e5-"));
            assert_eq!(m.files.each_ref().map(|f| f.name), ["config.json", "tokenizer.json", "model.safetensors"]);
            assert!(m.files.iter().all(|f| f.sha256.len() == 64 && f.sha256.bytes().all(|b| b.is_ascii_hexdigit())));
            assert_eq!(get(m.name).unwrap().dims, m.dims);
        }
        assert!(matches!(get("e5-large"), Err(Error::Usage(_))));
        assert!(default_dir(&MODELS[0]).unwrap().ends_with(Path::new("models").join("multilingual-e5-small")));
    }

    #[test]
    fn default_and_remembered_model() {
        let c = |r, s| choose(r, s).unwrap().name;
        assert_eq!(c(None, None), "e5-base", "new index");
        assert_eq!(c(None, Some("e5-small")), "e5-small", "remembered");
        assert_eq!(c(None, Some("e5-base")), "e5-base");
        assert_eq!(c(Some("e5-small"), None), "e5-small");
        assert_eq!(c(Some("e5-small"), Some("e5-base")), "e5-small", "explicit; check_model decides");
        assert!(matches!(choose(None, Some("e5-large")), Err(Error::Usage(_))));
    }

    #[test]
    fn sha256_known_value() {
        let d = tmp("sha");
        std::fs::write(d.join("x"), b"abc").unwrap();
        assert_eq!(
            sha256_file(&d.join("x")).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    const ABC: ModelFile = ModelFile {
        name: "config.json",
        size: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    };

    #[test]
    fn part_is_installed_only_when_hash_matches() {
        let d = tmp("part");
        let (part, dest) = (d.join("config.json.part"), d.join("config.json"));
        std::fs::write(&part, b"abd").unwrap();
        assert!(matches!(install_part(&MODELS[0], &ABC, &part, &dest), Err(Error::Format(_))));
        assert!(!part.exists() && !dest.exists(), "bad download removed, nothing installed");
        std::fs::write(&part, b"abc").unwrap();
        install_part(&MODELS[0], &ABC, &part, &dest).unwrap();
        assert!(!part.exists() && std::fs::read(&dest).unwrap() == b"abc");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn verify_and_list_report_missing_files() {
        let d = tmp("verify");
        assert!(matches!(verify(&MODELS[0], &d), Err(Error::NotFound(_))));
        std::fs::write(d.join("config.json"), b"{}").unwrap();
        assert!(matches!(verify(&MODELS[0], &d), Err(Error::Format(_))), "wrong size");
        let v = list(|_| Ok(d.clone())).unwrap();
        assert_eq!(v["models"].as_array().unwrap().len(), 2);
        assert_eq!(v["models"][0]["ready"], false);
        assert_eq!(v["models"][0]["files"][0]["present"], false);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
