//! Embedding worker processes. One process with more threads scales badly;
//! several processes on few threads each scale well (measured), so
//! `--workers N` (N > 1) runs N child `outlook-index embed-worker`
//! processes, each with its own copy of the model. The parent stays the only
//! DB writer.
//!
//! Protocol on the child's stdin/stdout, every message one frame: u32 LE
//! payload length, then the payload.
//! - child -> parent, once loaded: a response with no vectors (ready) or an error;
//! - parent -> child: a JSON array of passage texts (no prefix);
//! - child -> parent: byte 0 then n x dims f32 LE (input order), or byte 1
//!   then JSON `{"kind","message"}`.
//!
//! The child exits at end of input. Nothing else is written on its stdout.

use std::io::{BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use super::embed::{logical_cpus, synthetic_passages};
use super::model::Model;
use super::vectors::{Embed, from_blob, to_blob};
use crate::error::{Error, Result, guarded};

/// Larger frames are refused (a batch is a few MB at most).
const MAX_FRAME: usize = 256 << 20;

fn io(e: std::io::Error) -> Error {
    Error::Io(format!("embedding worker: {e}"))
}

pub(crate) fn write_frame(w: &mut impl Write, payload: &[u8]) -> Result<()> {
    let n = u32::try_from(payload.len()).map_err(|_| Error::Format("frame too large".into()))?;
    w.write_all(&n.to_le_bytes()).and_then(|_| w.write_all(payload)).and_then(|_| w.flush()).map_err(io)
}

/// Next frame; None at end of input between frames. End of input inside a
/// frame is a protocol (format) error.
pub(crate) fn read_frame(r: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut n = Vec::with_capacity(4);
    r.by_ref().take(4).read_to_end(&mut n).map_err(io)?;
    match n.len() {
        0 => return Ok(None),
        4 => {}
        k => return Err(Error::Format(format!("truncated frame header ({k} of 4 bytes)"))),
    }
    let n = u32::from_le_bytes(n.try_into().unwrap()) as usize;
    if n > MAX_FRAME {
        return Err(Error::Format(format!("frame of {n} bytes")));
    }
    let mut b = Vec::with_capacity(n);
    r.by_ref().take(n as u64).read_to_end(&mut b).map_err(io)?;
    if b.len() < n {
        return Err(Error::Format(format!("truncated frame ({} of {n} bytes)", b.len())));
    }
    Ok(Some(b))
}

fn message(e: &Error) -> &str {
    match e {
        Error::Io(m)
        | Error::Format(m)
        | Error::Corrupt(m)
        | Error::NotFound(m)
        | Error::Usage(m)
        | Error::UpgradePending(m) => m,
    }
}

fn response(r: Result<Vec<u8>>) -> Vec<u8> {
    match r {
        Ok(blobs) => [&[0u8][..], &blobs].concat(),
        Err(e) => [&[1u8][..], json!({ "kind": e.kind(), "message": message(&e) }).to_string().as_bytes()].concat(),
    }
}

/// `n` vectors of `dims`, or the child's error with its kind.
fn decode(b: &[u8], n: usize, dims: usize) -> Result<Vec<Vec<f32>>> {
    match b.split_first() {
        Some((0, v)) if v.len() == n * dims * 4 => v.chunks(dims * 4).map(|c| from_blob(c, dims)).collect(),
        Some((1, e)) => {
            let e: Json = serde_json::from_slice(e).map_err(|e| Error::Format(format!("worker error: {e}")))?;
            let m = e["message"].as_str().unwrap_or_default().to_string();
            Err(match e["kind"].as_str() {
                Some("usage") => Error::Usage(m),
                Some("not_found") => Error::NotFound(m),
                Some("io") => Error::Io(m),
                Some("inconsistent") => Error::Corrupt(m),
                _ => Error::Format(m),
            })
        }
        _ => Err(Error::Format(format!("worker reply of {} bytes for {n} vectors", b.len()))),
    }
}

/// Child side: report the load result, then embed each request until end of
/// input. Errors go back in the reply; only I/O errors end the loop.
pub fn serve<E: Embed>(e: Result<E>, r: &mut impl Read, w: &mut impl Write) -> Result<()> {
    let mut e = match e {
        Ok(e) => e,
        Err(err) => return write_frame(w, &response(Err(err))),
    };
    write_frame(w, &response(Ok(Vec::new())))?;
    let dims = e.model().dims;
    while let Some(req) = read_frame(r)? {
        let reply = guarded(|| {
            let texts: Vec<String> =
                serde_json::from_slice(&req).map_err(|e| Error::Format(format!("worker request: {e}")))?;
            let vecs = e.passages(&texts.iter().map(String::as_str).collect::<Vec<_>>())?;
            if vecs.len() != texts.len() {
                return Err(Error::Format(format!("{} vectors for {} passages", vecs.len(), texts.len())));
            }
            vecs.iter().try_fold(Vec::with_capacity(vecs.len() * dims * 4), |mut b, v| {
                b.extend(to_blob(v, dims)?);
                Ok(b)
            })
        });
        write_frame(w, &response(reply))?;
    }
    Ok(())
}

/// Parent side of one worker process.
pub struct Child {
    proc: std::process::Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    model: &'static Model,
}

impl Child {
    /// Start `n` workers from `cmd` (the `embed-worker` command line) and
    /// wait until all have loaded the model; the first load error is returned.
    pub fn spawn(mut cmd: Command, n: usize, model: &'static Model) -> Result<Vec<Child>> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let mut proc = cmd.spawn().map_err(io)?;
            let (stdin, stdout) = (proc.stdin.take().unwrap(), BufReader::new(proc.stdout.take().unwrap()));
            out.push(Child { proc, stdin, stdout, model });
        }
        for c in &mut out {
            let ready = read_frame(&mut c.stdout)?.ok_or_else(|| Error::Io("embedding worker exited".into()))?;
            decode(&ready, 0, model.dims)?;
        }
        Ok(out)
    }
}

impl Embed for Child {
    fn model(&self) -> &'static Model {
        self.model
    }
    fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        write_frame(&mut self.stdin, serde_json::to_string(texts).unwrap().as_bytes())?;
        let reply = read_frame(&mut self.stdout)?.ok_or_else(|| Error::Io("embedding worker exited".into()))?;
        decode(&reply, texts.len(), self.model.dims)
    }
}

impl Drop for Child {
    /// Holds no state: stopping it mid-batch loses nothing stored.
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

/// `bench-embed --workers N`: synthetic batches through every worker at
/// once for about `time`; aggregate passages per second.
pub fn bench<E: Embed + ?Sized>(
    es: &mut [&mut E],
    load: Duration,
    time: Duration,
    batch: usize,
    threads: usize,
) -> Result<Json> {
    let pool = synthetic_passages(64);
    let n = es.len().max(1);
    let t = Instant::now();
    let done: Vec<Result<(usize, usize)>> = std::thread::scope(|s| {
        let pool = &pool;
        let hs: Vec<_> = es
            .iter_mut()
            .enumerate()
            .map(|(w, e)| {
                s.spawn(move || {
                    let (mut passages, mut chars, mut i) = (0, 0, w * batch);
                    while passages == 0 || t.elapsed() < time {
                        let texts: Vec<&str> = (0..batch).map(|k| pool[(i + k) % pool.len()].as_str()).collect();
                        chars += texts.iter().map(|t| t.chars().count()).sum::<usize>();
                        e.passages(&texts)?;
                        passages += batch;
                        i += batch * n;
                    }
                    Ok((passages, chars))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(Error::Format("worker panicked".into())))).collect()
    });
    let (mut passages, mut chars) = (0, 0);
    for r in done {
        let (p, c) = r?;
        (passages, chars) = (passages + p, chars + c);
    }
    let secs = t.elapsed().as_secs_f64();
    let m = es.first().map(|e| e.model()).ok_or_else(|| Error::Usage("no worker".into()))?;
    Ok(json!({
        "model": m.name,
        "revision": m.revision,
        "dims": m.dims,
        "workers": n,
        "threads": threads,
        "cpus": logical_cpus(),
        "batch": batch,
        "load_ms": load.as_millis() as u64,
        "passages": passages,
        "avg_chars": chars / passages.max(1),
        "seconds": (secs * 10.0).round() / 10.0,
        "passages_per_s": (passages as f64 / secs * 100.0).round() / 100.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::model::MODELS;

    /// Fake: vector = [len, 0, ...]; "boom" fails with a usage error.
    struct Len;

    impl Embed for Len {
        fn model(&self) -> &'static Model {
            &MODELS[0]
        }
        fn passages(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if texts.contains(&"boom") {
                return Err(Error::Usage("boom".into()));
            }
            Ok(texts.iter().map(|t| [vec![t.len() as f32 + 0.1], vec![0.0; 383]].concat()).collect())
        }
    }

    fn request(texts: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        write_frame(&mut b, serde_json::to_string(texts).unwrap().as_bytes()).unwrap();
        b
    }

    #[test]
    fn protocol_round_trip_and_errors() {
        let input = [request(&["ab", "c"]), request(&["boom"]), request(&[])].concat();
        let mut out = Vec::new();
        serve(Ok(Len), &mut input.as_slice(), &mut out).unwrap();
        let mut r = out.as_slice();
        let mut next = || read_frame(&mut r).unwrap().unwrap();
        assert!(decode(&next(), 0, 384).unwrap().is_empty(), "ready");
        let v = decode(&next(), 2, 384).unwrap();
        assert_eq!((v[0][0], v[1][0], v[1].len()), (2.1, 1.1, 384), "exact f32, input order");
        let e = decode(&next(), 1, 384).unwrap_err();
        assert_eq!((e.kind(), e.to_string().as_str()), ("usage", "boom"), "kind and message kept");
        assert!(decode(&next(), 0, 384).unwrap().is_empty());
        assert!(read_frame(&mut r).unwrap().is_none(), "ends with the input");
        assert_eq!(decode(&[0, 1, 2], 1, 384).unwrap_err().kind(), "format", "short reply");

        // A load error is the only reply.
        let mut out = Vec::new();
        serve::<Len>(Err(Error::NotFound("model files".into())), &mut input.as_slice(), &mut out).unwrap();
        let mut r = out.as_slice();
        assert_eq!(decode(&read_frame(&mut r).unwrap().unwrap(), 0, 384).unwrap_err().kind(), "not_found");
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    /// N3: EOF is clean only at a frame boundary.
    #[test]
    fn frame_truncation_and_size() {
        let mut ok = Vec::new();
        write_frame(&mut ok, b"abc").unwrap();
        let mut r = ok.as_slice();
        assert_eq!(read_frame(&mut r).unwrap().as_deref(), Some(&b"abc"[..]));
        assert!(read_frame(&mut r).unwrap().is_none(), "EOF at a boundary");
        for cut in 1..4 {
            assert_eq!(read_frame(&mut &ok[..cut]).unwrap_err().kind(), "format", "{cut}-byte header");
            let two = [&ok[..], &ok[..cut]].concat();
            let mut r = two.as_slice();
            read_frame(&mut r).unwrap();
            assert_eq!(read_frame(&mut r).unwrap_err().kind(), "format", "{cut}-byte header after a frame");
        }
        assert_eq!(read_frame(&mut &ok[..5]).unwrap_err().kind(), "format", "short payload");
        let big = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert_eq!(read_frame(&mut &big[..]).unwrap_err().kind(), "format", "oversized");
    }

    #[test]
    fn bench_sums_workers() {
        let (mut a, mut b) = (Len, Len);
        let v = bench(&mut [&mut a, &mut b], Duration::ZERO, Duration::ZERO, 3, 2).unwrap();
        assert_eq!((v["workers"].as_u64(), v["passages"].as_u64(), v["threads"].as_u64()), (Some(2), Some(6), Some(2)));
    }
}
