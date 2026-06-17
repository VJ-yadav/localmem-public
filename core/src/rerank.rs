//! Local ONNX cross-encoder reranker (Phase 2 / T-74b).
//!
//! Second-stage retrieval. The fast first stage (BM25 + vector, fused by RRF)
//! *recalls* candidates; this cross-encoder *rescores* the top-N by true
//! query-document relevance and reorders them. Unlike the bi-encoder
//! [`crate::embed::Embedder`] (which embeds query and document independently
//! and compares vectors), a cross-encoder feeds `query [SEP] doc` through the
//! model together, so it captures fine-grained interaction and is markedly more
//! precise. The cost is that it must run once per query-doc pair, which is why
//! it only ever runs over the recalled top-N, never the whole store.
//!
//! Default model: `cross-encoder/ms-marco-MiniLM-L-6-v2` exported to ONNX
//! (sequence-classification head, a single relevance logit per pair). Same
//! on-disk layout as the embedder: `model.onnx` + `tokenizer.json` in a model
//! directory, fetched via `localmem fetch-model`.

use anyhow::{anyhow, Context, Result};
use ndarray::Array2;
use ort::session::Session;
use ort::value::Value;
use std::path::Path;
use tokenizers::tokenizer::Tokenizer;
use tokenizers::utils::padding::{PaddingDirection, PaddingParams, PaddingStrategy};
use tokenizers::utils::truncation::{TruncationDirection, TruncationParams, TruncationStrategy};

/// Map an `ort::Error` into an `anyhow::Error` (ort's error type is not
/// `std::error::Error`, so `?`/`context` aren't available at ort boundaries).
fn ort_err(label: &str) -> impl FnOnce(ort::Error) -> anyhow::Error + '_ {
    move |e| anyhow!("{label}: {e}")
}

const MAX_SEQUENCE_LENGTH: usize = 512;

/// On-disk filenames expected inside the reranker model directory. Mirrors the
/// embedder layout so `fetch-model` and tests share one convention.
pub const MODEL_FILENAME: &str = "model.onnx";
pub const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Local cross-encoder reranker backed by ONNX Runtime + a HuggingFace
/// tokenizer. `Session::run` needs `&mut self`, so higher layers wrap this in a
/// mutex (the retriever holds it behind `Arc<Mutex<Option<Reranker>>>`).
pub struct Reranker {
    session: Session,
    tokenizer: Tokenizer,
    max_length: usize,
}

impl std::fmt::Debug for Reranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reranker")
            .field("max_length", &self.max_length)
            .finish_non_exhaustive()
    }
}

impl Reranker {
    /// Load the cross-encoder from `model_dir` (must contain `model.onnx` and
    /// `tokenizer.json`). Returns an error if either is missing so the caller
    /// can degrade to no-rerank rather than fail the query.
    pub fn load(model_dir: impl AsRef<Path>) -> Result<Self> {
        let dir = model_dir.as_ref();
        let model_path = dir.join(MODEL_FILENAME);
        let tokenizer_path = dir.join(TOKENIZER_FILENAME);
        if !model_path.exists() {
            return Err(anyhow!(
                "missing reranker model file: {}",
                model_path.display()
            ));
        }
        if !tokenizer_path.exists() {
            return Err(anyhow!(
                "missing reranker tokenizer file: {}",
                tokenizer_path.display()
            ));
        }
        let tokenizer = load_tokenizer(&tokenizer_path)
            .context("load reranker tokenizer from tokenizer.json")?;
        let model_bytes = std::fs::read(&model_path)
            .with_context(|| format!("read reranker ONNX bytes from {}", model_path.display()))?;
        let session = Session::builder()
            .map_err(ort_err("create ort session builder"))?
            .commit_from_memory(&model_bytes)
            .map_err(ort_err(&format!(
                "load reranker ONNX from {}",
                model_path.display()
            )))?;
        Ok(Self {
            session,
            tokenizer,
            max_length: MAX_SEQUENCE_LENGTH,
        })
    }

    /// Score each `(query, doc)` pair; higher = more relevant. Returns one
    /// score per `docs` entry, in the same order. Empty input -> empty output.
    pub fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        // Encode (query, doc) PAIRS so token_type_ids mark the two segments.
        let pairs: Vec<(String, String)> = docs
            .iter()
            .map(|d| (query.to_string(), (*d).to_string()))
            .collect();
        let encodings = self
            .tokenizer
            .encode_batch(pairs, /* add_special_tokens = */ true)
            .map_err(|e| anyhow!("tokenize rerank pairs: {e}"))?;

        let batch = encodings.len();
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);
        if seq_len == 0 {
            return Err(anyhow!("reranker tokenizer produced empty encoding"));
        }
        if seq_len > self.max_length {
            return Err(anyhow!(
                "reranker tokenizer returned {seq_len} tokens, exceeding max {}",
                self.max_length
            ));
        }

        let mut input_ids = Array2::<i64>::zeros((batch, seq_len));
        let mut attention_mask = Array2::<i64>::zeros((batch, seq_len));
        let mut token_type_ids = Array2::<i64>::zeros((batch, seq_len));
        for (i, enc) in encodings.iter().enumerate() {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let types = enc.get_type_ids();
            for j in 0..seq_len {
                input_ids[[i, j]] = ids[j] as i64;
                attention_mask[[i, j]] = mask[j] as i64;
                token_type_ids[[i, j]] = types[j] as i64;
            }
        }

        let input_ids_v =
            Value::from_array(input_ids).map_err(ort_err("build input_ids tensor"))?;
        let attn_v =
            Value::from_array(attention_mask).map_err(ort_err("build attention_mask tensor"))?;
        let types_v =
            Value::from_array(token_type_ids).map_err(ort_err("build token_type_ids tensor"))?;

        let outputs = self
            .session
            .run(ort::inputs! {
                "input_ids" => input_ids_v,
                "attention_mask" => attn_v,
                "token_type_ids" => types_v,
            })
            .map_err(ort_err("reranker session run"))?;

        // Sequence-classification head: output "logits" of shape
        // [batch, num_labels]. ms-marco cross-encoders use num_labels = 1 (a
        // single relevance score); we take the first label as the score, which
        // is also correct for a 2-label setup where label 0 is "relevant".
        let logits = outputs
            .get("logits")
            .ok_or_else(|| anyhow!("reranker output missing `logits`"))?;
        let (shape, data) = logits
            .try_extract_tensor::<f32>()
            .map_err(ort_err("extract logits as f32 tensor"))?;
        let num_labels = if shape.len() == 2 {
            (shape[1] as usize).max(1)
        } else {
            1
        };
        let mut out = Vec::with_capacity(batch);
        for i in 0..batch {
            out.push(data[i * num_labels]);
        }
        Ok(out)
    }
}

/// Reorder `items` by their parallel `scores` (descending) and truncate to `k`.
/// Pure and model-free: the testable core of reranking. `scores[i]` is the
/// rerank score for `items[i]`; lengths should match (missing scores sort
/// last).
pub fn reorder_by_scores<T>(items: Vec<T>, scores: &[f32], k: usize) -> Vec<T> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|&a, &b| {
        let sa = scores.get(a).copied().unwrap_or(f32::MIN);
        let sb = scores.get(b).copied().unwrap_or(f32::MIN);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    idx.into_iter()
        .take(k)
        .filter_map(|i| slots[i].take())
        .collect()
}

fn load_tokenizer(path: &Path) -> Result<Tokenizer> {
    let mut t = Tokenizer::from_file(path)
        .map_err(|e| anyhow!("read reranker tokenizer.json at {}: {e}", path.display()))?;
    t.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        direction: PaddingDirection::Right,
        pad_to_multiple_of: None,
        pad_id: 0,
        pad_type_id: 0,
        pad_token: "[PAD]".to_string(),
    }));
    t.with_truncation(Some(TruncationParams {
        max_length: MAX_SEQUENCE_LENGTH,
        strategy: TruncationStrategy::LongestFirst,
        stride: 0,
        direction: TruncationDirection::Right,
    }))
    .map_err(|e| anyhow!("configure truncation on reranker tokenizer: {e}"))?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_by_scores_sorts_descending_and_truncates() {
        let items = vec!["a", "b", "c", "d"];
        // b is most relevant, then d, then a, then c.
        let scores = vec![0.2f32, 0.9, 0.1, 0.5];
        assert_eq!(reorder_by_scores(items.clone(), &scores, 2), vec!["b", "d"]);
        assert_eq!(
            reorder_by_scores(items, &scores, 10),
            vec!["b", "d", "a", "c"]
        );
    }

    #[test]
    fn reorder_by_scores_handles_empty_and_missing() {
        assert!(reorder_by_scores(Vec::<&str>::new(), &[], 5).is_empty());
        // Fewer scores than items: the unscored item sorts last.
        let items = vec!["x", "y"];
        let scores = vec![0.5f32];
        assert_eq!(reorder_by_scores(items, &scores, 5), vec!["x", "y"]);
    }
}
