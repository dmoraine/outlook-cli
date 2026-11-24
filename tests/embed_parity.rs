//! e5 parity with the Python reference (tests/fixtures/e5_reference.json, made
//! by scripts/e5_reference.py) and semantic sanity. Needs the model files, so
//! it is ignored by default:
//!   E5_SMALL_DIR=... E5_BASE_DIR=... cargo test --release --features index --test embed_parity -- --ignored --nocapture
//! Without the variables, the default model directories are used.
#![cfg(feature = "index")]

use outlook_cli::index::embed::{Embedder, Kind};
use outlook_cli::index::model;
use serde_json::Value;

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn parity(name: &str, env: &str) {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/e5_reference.json")).unwrap();
    let r = &fixture["models"][name];
    let m = model::get(name).unwrap();
    assert_eq!(r["revision"], m.revision, "fixture made for another revision");
    let dir = std::env::var_os(env).map(Into::into).unwrap_or_else(|| model::default_dir(m).unwrap());
    let e = Embedder::load(m, &dir).unwrap_or_else(|err| panic!("{name}: {err} ({env})"));
    let items = r["items"].as_array().unwrap();
    let kind = |i: &Value| if i["kind"] == "query" { Kind::Query } else { Kind::Passage };
    let want: Vec<Vec<f32>> = items
        .iter()
        .map(|i| i["embedding"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect())
        .collect();

    let mut ids_equal = 0;
    let mut got = Vec::new();
    for i in items {
        let ids: Vec<u32> = i["ids"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as u32).collect();
        let text = i["text"].as_str().unwrap();
        let mine = e.token_ids(kind(i), text).unwrap();
        assert!(mine.len() <= 512);
        ids_equal += usize::from(mine == ids);
        let v = if kind(i) == Kind::Query { e.embed_queries(&[text]) } else { e.embed_passages(&[text]) };
        got.push(v.unwrap().remove(0));
    }
    assert_eq!(ids_equal, items.len(), "{name}: token ids differ from the reference tokenizer");
    let single: Vec<f32> = got.iter().zip(&want).map(|(g, w)| cos(g, w)).collect();

    // All passages in one padded, length-sorted batch.
    let pi: Vec<usize> = (0..items.len()).filter(|&k| kind(&items[k]) == Kind::Passage).collect();
    let texts: Vec<&str> = pi.iter().map(|&k| items[k]["text"].as_str().unwrap()).collect();
    let batched: Vec<f32> = e.embed_passages(&texts).unwrap().iter().zip(&pi).map(|(g, &k)| cos(g, &want[k])).collect();

    let min = |v: &[f32]| v.iter().copied().fold(1.0f32, f32::min);
    eprintln!(
        "{{\"model\":\"{name}\",\"items\":{},\"ids_equal\":{ids_equal},\"min_cos_single\":{:.7},\"min_cos_batched\":{:.7}}}",
        items.len(),
        min(&single),
        min(&batched)
    );
    assert!(min(&single) > 0.9999 && min(&batched) > 0.9999, "{name}: {single:?} {batched:?}");

    // Semantic sanity (synthetic texts, see the fixture): 0 password query,
    // 1/2 password passages EN/FR, 3 unrelated, 4 budget query, 5 budget FR.
    assert!(cos(&got[0], &got[1]) > cos(&got[0], &got[3]), "{name}: EN match");
    assert!(cos(&got[0], &got[2]) > cos(&got[0], &got[3]), "{name}: cross-lingual match");
    assert!(cos(&got[4], &got[5]) > cos(&got[4], &got[3]), "{name}: FR match");
    // A French query is closer to its French and English passages than to the unrelated one.
    let fr = &e.embed_queries(&["comment réinitialiser mon mot de passe ?"]).unwrap()[0];
    let (en_p, fr_p, other) = (cos(fr, &got[1]), cos(fr, &got[2]), cos(fr, &got[3]));
    eprintln!(
        "{{\"model\":\"{name}\",\"fr_query_cos\":{{\"en\":{en_p:.4},\"fr\":{fr_p:.4},\"unrelated\":{other:.4}}}}}"
    );
    assert!(en_p > other && fr_p > other, "{name}: FR query");
    // The prefix changes the vector: query and passage of the same text differ.
    let t = "quarterly budget";
    let (q, p) = (e.embed_queries(&[t]).unwrap(), e.embed_passages(&[t]).unwrap());
    assert!(cos(&q[0], &p[0]) < 0.999, "{name}: prefixes applied");
}

#[test]
#[ignore = "needs the e5-small model files"]
fn e5_small_parity() {
    parity("e5-small", "E5_SMALL_DIR");
}

#[test]
#[ignore = "needs the e5-base model files"]
fn e5_base_parity() {
    parity("e5-base", "E5_BASE_DIR");
}
