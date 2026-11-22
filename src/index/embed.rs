//! Local CPU embeddings with multilingual e5 (candle, pure Rust): e5 prefixes,
//! 512-token truncation, length-sorted batches, masked mean pooling, L2 norm.

use std::path::Path;
use std::time::{Duration, Instant};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::{bert, xlm_roberta};
use serde_json::{Value as Json, json};
use tokenizers::{Tokenizer, TruncationParams};

use super::model::{self, Model};
use crate::error::{Error, Result};

/// Model input limit, special tokens included.
pub const MAX_TOKENS: usize = 512;
pub const DEFAULT_BATCH: usize = 8;

/// The e5 models need a prefix telling queries from indexed passages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Query,
    Passage,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Query => "query: ",
            Kind::Passage => "passage: ",
        }
    }
}

/// Model input text: prefix + trimmed text, trimmed again like
/// sentence-transformers does (an empty text gives "query:").
pub fn prefixed(kind: Kind, text: &str) -> String {
    format!("{}{}", kind.prefix(), text.trim()).trim_end().to_string()
}

/// Compute threads per worker. One process scales badly past ~3 threads;
/// more throughput comes from more worker processes.
pub const DEFAULT_THREADS: usize = 3;

/// Workers for `cpus` logical CPUs: (cpus - 2) / 3, at least 1, so two CPUs
/// stay free (14 CPUs: 4 workers x 3 threads; 4 CPUs: 1 x 3).
pub fn default_workers(cpus: usize) -> usize {
    (cpus.saturating_sub(2) / DEFAULT_THREADS).max(1)
}

pub fn logical_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

fn ml(e: impl std::fmt::Display) -> Error {
    Error::Format(format!("embedding model: {e}"))
}

/// e5-small is a BERT, e5-base an XLM-RoBERTa (position ids from the padding id).
enum Net {
    Bert(bert::BertModel),
    XlmR(xlm_roberta::XLMRobertaModel),
}

impl Net {
    /// From config.json; returns the net and its hidden size.
    fn load(config: &str, vb: VarBuilder) -> Result<(Net, usize)> {
        let v: Json = serde_json::from_str(config).map_err(ml)?;
        match v["model_type"].as_str() {
            Some("bert") => {
                let c: bert::Config = serde_json::from_value(v).map_err(ml)?;
                Ok((Net::Bert(bert::BertModel::load(vb, &c).map_err(ml)?), c.hidden_size))
            }
            Some("xlm-roberta") => {
                let c: xlm_roberta::Config = serde_json::from_value(v).map_err(ml)?;
                Ok((Net::XlmR(xlm_roberta::XLMRobertaModel::new(&c, vb).map_err(ml)?), c.hidden_size))
            }
            t => Err(ml(format!("unsupported model_type {t:?}"))),
        }
    }

    /// (batch, tokens) ids and 0/1 mask -> (batch, tokens, hidden).
    fn forward(&self, ids: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
        let types = ids.zeros_like()?;
        match self {
            Net::Bert(m) => m.forward(ids, &types, Some(mask)),
            Net::XlmR(m) => m.forward(ids, mask, &types, None, None, None),
        }
    }
}

/// Mean of the unmasked token states, L2-normalized: (b, t, h), (b, t) -> (b, h).
fn pool(hidden: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
    let m = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
    let mean = hidden.broadcast_mul(&m)?.sum(1)?.broadcast_div(&m.sum(1)?)?;
    let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?.clamp(1e-12f32, f32::MAX)?;
    mean.broadcast_div(&norm)
}

/// Embed token id sequences in batches of similar length (less padding).
/// Output order = input order.
fn embed_ids(net: &Net, pad: u32, batch: usize, ids: &[Vec<u32>]) -> Result<Vec<Vec<f32>>> {
    let mut order: Vec<usize> = (0..ids.len()).collect();
    order.sort_by_key(|&i| ids[i].len());
    let mut out = vec![Vec::new(); ids.len()];
    for group in order.chunks(batch.max(1)) {
        let t = group.iter().map(|&i| ids[i].len()).max().unwrap_or(0);
        let (mut x, mut m) = (Vec::with_capacity(group.len() * t), Vec::with_capacity(group.len() * t));
        for &i in group {
            let n = ids[i].len();
            x.extend(ids[i].iter().copied().chain(std::iter::repeat_n(pad, t - n)));
            m.extend(std::iter::repeat_n(1u32, n).chain(std::iter::repeat_n(0, t - n)));
        }
        let shape = (group.len(), t);
        let x = Tensor::from_vec(x, shape, &Device::Cpu).map_err(ml)?;
        let m = Tensor::from_vec(m, shape, &Device::Cpu).map_err(ml)?;
        let v = net.forward(&x, &m).and_then(|h| pool(&h, &m)).and_then(|p| p.to_vec2::<f32>()).map_err(ml)?;
        for (&i, row) in group.iter().zip(v) {
            out[i] = row;
        }
    }
    Ok(out)
}

/// The embedding run moves it to a worker thread.
const _: () = {
    const fn send<T: Send>() {}
    send::<Embedder>()
};

pub struct Embedder {
    net: Net,
    tok: Tokenizer,
    pad: u32,
    pub model: &'static Model,
    pub batch: usize,
}

impl Embedder {
    /// Verify the files of `m` in `dir` against the pinned hashes, then load them.
    pub fn load(m: &'static Model, dir: &Path) -> Result<Embedder> {
        model::verify(m, dir)?;
        let config = std::fs::read_to_string(dir.join("config.json")).map_err(|e| Error::Io(e.to_string()))?;
        let mut tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(ml)?;
        tok.with_truncation(Some(TruncationParams { max_length: MAX_TOKENS, ..Default::default() })).map_err(ml)?;
        tok.with_padding(None);
        let pad = tok.token_to_id("<pad>").ok_or_else(|| ml("no <pad> token"))?;
        // SAFETY: read-only map of a file that was just verified; model files
        // are not modified while the tool runs.
        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, &Device::Cpu) }
                .map_err(ml)?;
        let (net, dims) = Net::load(&config, vb)?;
        if dims != m.dims {
            return Err(ml(format!("{dims} dimensions, expected {}", m.dims)));
        }
        Ok(Embedder { net, tok, pad, model: m, batch: DEFAULT_BATCH })
    }

    fn ids(&self, kind: Kind, texts: &[&str]) -> Result<Vec<Vec<u32>>> {
        let inputs: Vec<String> = texts.iter().map(|t| prefixed(kind, t)).collect();
        let enc = self.tok.encode_batch(inputs, true).map_err(ml)?;
        Ok(enc.iter().map(|e| e.get_ids().to_vec()).collect())
    }

    /// Token ids as fed to the model (prefix and special tokens included).
    pub fn token_ids(&self, kind: Kind, text: &str) -> Result<Vec<u32>> {
        Ok(self.ids(kind, &[text])?.remove(0))
    }

    fn embed(&self, kind: Kind, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        embed_ids(&self.net, self.pad, self.batch, &self.ids(kind, texts)?)
    }

    /// Unit vectors of `model.dims` floats, one per query ("query: " prefix).
    pub fn embed_queries(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed(Kind::Query, texts)
    }

    /// Unit vectors of `model.dims` floats, one per passage ("passage: " prefix).
    pub fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed(Kind::Passage, texts)
    }
}

/// Deterministic synthetic passages, chunked like mail bodies. No mail data.
pub fn synthetic_passages(n: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "budget",
        "meeting",
        "invoice",
        "delivery",
        "project",
        "report",
        "deadline",
        "contract",
        "review",
        "team",
        "client",
        "schedule",
        "update",
        "draft",
        "approval",
        "réunion",
        "facture",
        "livraison",
        "projet",
        "rapport",
        "échéance",
        "contrat",
        "équipe",
        "planning",
        "validation",
        "trimestre",
        "the",
        "a",
        "for",
        "with",
        "next",
        "le",
        "la",
        "pour",
        "avec",
        "prochain",
        "is",
        "est",
        "and",
        "et",
        "2026",
        "14h30",
        "Q3",
        "€1200",
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut word = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        WORDS[(seed % WORDS.len() as u64) as usize]
    };
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let paragraphs: Vec<String> = (0..12)
            .map(|_| {
                let sentences: Vec<String> =
                    (0..5).map(|_| (0..14).map(|_| word()).collect::<Vec<_>>().join(" ") + ".").collect();
                sentences.join(" ")
            })
            .collect();
        out.extend(super::text::chunk(&paragraphs.join("\n\n")).into_iter().map(|(t, _)| t));
    }
    out.truncate(n);
    out
}

/// `bench-embed` with one worker: passages per second on synthetic text
/// for about `time`, in this process.
pub fn bench(e: &Embedder, load: Duration, time: Duration, threads: usize) -> Result<Json> {
    let pool = synthetic_passages(64);
    let ids: Vec<Vec<u32>> = e.ids(Kind::Passage, &pool.iter().map(String::as_str).collect::<Vec<_>>())?;
    e.embed_queries(&["synthetic warm-up query"])?;
    let t = Instant::now();
    e.embed_queries(&["quarterly budget meeting next week"])?;
    let query = t.elapsed();
    let (mut passages, mut tokens, mut chars) = (0usize, 0usize, 0usize);
    let t = Instant::now();
    let mut i = 0;
    while passages == 0 || t.elapsed() < time {
        let batch: Vec<Vec<u32>> = (0..e.batch).map(|k| ids[(i + k) % ids.len()].clone()).collect();
        chars += (0..e.batch).map(|k| pool[(i + k) % pool.len()].chars().count()).sum::<usize>();
        tokens += batch.iter().map(Vec::len).sum::<usize>();
        embed_ids(&e.net, e.pad, e.batch, &batch)?;
        passages += batch.len();
        i += e.batch;
    }
    let secs = t.elapsed().as_secs_f64();
    let ms = |d: Duration| d.as_millis() as u64;
    Ok(json!({
        "model": e.model.name,
        "revision": e.model.revision,
        "dims": e.model.dims,
        "workers": 1,
        "threads": threads,
        "cpus": logical_cpus(),
        "batch": e.batch,
        "load_ms": ms(load),
        "query_ms": ms(query),
        "passages": passages,
        "avg_chars": chars / passages,
        "avg_tokens": tokens / passages,
        "seconds": (secs * 10.0).round() / 10.0,
        "passages_per_s": (passages as f64 / secs * 100.0).round() / 100.0,
        "tokens_per_s": (tokens as f64 / secs).round(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_nn::VarMap;

    #[test]
    fn prefixes_are_mandatory() {
        assert_eq!(prefixed(Kind::Query, "  how to reset? \n"), "query: how to reset?");
        assert_eq!(prefixed(Kind::Passage, "Budget Q3"), "passage: Budget Q3");
        assert_eq!(prefixed(Kind::Query, " \t"), "query:");
    }

    #[test]
    fn workers_default() {
        let w: Vec<usize> = [1, 2, 4, 5, 8, 14, 16].into_iter().map(default_workers).collect();
        assert_eq!(w, [1, 1, 1, 1, 2, 4, 4]);
        assert_eq!(DEFAULT_THREADS, 3);
    }

    #[test]
    fn pool_masks_and_normalizes() {
        // Two sequences of 3 tokens, hidden 2; the second has one padded token.
        let h =
            Tensor::new(&[[[1f32, 0.], [3., 0.], [0., 4.]], [[0., 2.], [0., 2.], [100., 100.]]], &Device::Cpu).unwrap();
        let m = Tensor::new(&[[1u32, 1, 1], [1, 1, 0]], &Device::Cpu).unwrap();
        let v = pool(&h, &m).unwrap().to_vec2::<f32>().unwrap();
        // mean (4/3, 4/3) -> (0.7071, 0.7071); mean (0, 2) -> (0, 1), padding ignored.
        let close = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5);
        assert!(close(&v[0], &[0.70710677, 0.70710677]), "{v:?}");
        assert!(close(&v[1], &[0., 1.]), "{v:?}");
        let z = pool(&Tensor::zeros((1, 3, 2), DType::F32, &Device::Cpu).unwrap(), &m.narrow(0, 0, 1).unwrap());
        assert!(z.unwrap().to_vec2::<f32>().unwrap()[0].iter().all(|x| x.is_finite()), "zero vector stays finite");
    }

    fn tiny(model_type: &str) -> Net {
        let cfg = json!({
            "model_type": model_type, "vocab_size": 50, "hidden_size": 16, "num_hidden_layers": 2,
            "num_attention_heads": 2, "intermediate_size": 32, "hidden_act": "gelu", "hidden_dropout_prob": 0.0,
            "attention_probs_dropout_prob": 0.0, "max_position_embeddings": 40, "type_vocab_size": 1,
            "initializer_range": 0.02, "layer_norm_eps": 1e-5, "pad_token_id": 1,
            "position_embedding_type": "absolute", "classifier_dropout": null,
        });
        let vm = VarMap::new();
        let (net, dims) = Net::load(&cfg.to_string(), VarBuilder::from_varmap(&vm, DType::F32, &Device::Cpu)).unwrap();
        assert_eq!(dims, 16);
        net
    }

    /// Random tiny weights, no download: padding in a batch must not change a
    /// vector, and output order must match input order.
    #[test]
    fn batching_is_padding_invariant() {
        for arch in ["bert", "xlm-roberta"] {
            let net = tiny(arch);
            let seqs = vec![vec![0, 5, 6, 7, 8, 9, 10, 2], vec![0, 11, 2], vec![0, 12, 13, 14, 2]];
            let alone: Vec<Vec<f32>> =
                seqs.iter().map(|s| embed_ids(&net, 1, 1, std::slice::from_ref(s)).unwrap().remove(0)).collect();
            let batched = embed_ids(&net, 1, 8, &seqs).unwrap();
            for (a, b) in alone.iter().zip(&batched) {
                assert_eq!(a.len(), 16);
                let norm: f32 = a.iter().map(|x| x * x).sum();
                assert!((norm - 1.0).abs() < 1e-4, "{arch}: unit vector");
                let cos: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                assert!(cos > 0.9999, "{arch}: cos {cos}");
            }
            assert!(alone[0].iter().zip(&alone[1]).map(|(x, y)| x * y).sum::<f32>() < 0.9999, "{arch}: distinct");
        }
        assert!(Net::load(r#"{"model_type":"gpt2"}"#, VarBuilder::zeros(DType::F32, &Device::Cpu)).is_err());
    }

    #[test]
    fn synthetic_passages_are_chunk_sized_and_stable() {
        let p = synthetic_passages(10);
        assert_eq!(p.len(), 10);
        assert_eq!(p, synthetic_passages(10));
        assert!(p.iter().all(|t| (200..=1750).contains(&t.chars().count())), "{:?}", p.iter().map(|t| t.len()));
    }
}
