//! Local ONNX-runtime embedder.
//!
//! Default model: BGE-small-en-v1.5 via the `ort` crate. See ARCHITECTURE.md
//! (Derived stores -> `vectors.lance/`) and TASKS.md task T-08.
//!
//! The embedder owns an ONNX [`Session`] plus a [`Tokenizer`] pinned to the
//! BGE vocabulary. Both load eagerly so [`Embedder::embed`] is a pure compute
//! call with no I/O on the hot path.
//!
//! Pooling: BGE-small recommends taking the `[CLS]` token (position 0 of the
//! last hidden state) followed by L2 normalization. Mean pooling would also
//! work but produces materially different vectors; sticking with CLS keeps
//! us bit-compatible with the model card and with `fastembed`.

use anyhow::{anyhow, Context, Result};
use ndarray::Array2;
use ort::session::Session;
use ort::value::Value;
use std::path::Path;
use tokenizers::tokenizer::Tokenizer;
use tokenizers::utils::padding::{PaddingDirection, PaddingParams, PaddingStrategy};
use tokenizers::utils::truncation::{TruncationDirection, TruncationParams, TruncationStrategy};

/// Map an `ort::Error` into an `anyhow::Error` carrying its `Display` text.
/// `ort::Error` does not implement `std::error::Error`, so neither `?` nor
/// `anyhow::Context::context` are available; this helper centralizes the
/// conversion at every `ort` API boundary.
fn ort_err(label: &str) -> impl FnOnce(ort::Error) -> anyhow::Error + '_ {
    move |e| anyhow!("{label}: {e}")
}

/// Embedding width emitted by BGE-small-en-v1.5.
pub const EMBEDDING_DIM: usize = 384;

/// Hard upper bound on input tokens. BGE-small inherits BERT's 512 cap.
const MAX_SEQUENCE_LENGTH: usize = 512;

/// On-disk filenames expected inside the model directory passed to
/// [`Embedder::load`]. Kept as constants so callers (and tests) can reuse
/// them without string-typo risk.
pub const MODEL_FILENAME: &str = "model.onnx";
pub const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Local embedder backed by ONNX Runtime and a HuggingFace tokenizer.
///
/// One instance per process is sufficient. [`Session`] is thread-safe under
/// `ort` 2.x but the Rust API requires `&mut self` for `Session::run`, so
/// concurrent callers should wrap the embedder in a mutex (or build a small
/// pool) at a higher layer.
pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    dim: usize,
    max_length: usize,
}

impl std::fmt::Debug for Embedder {
    // Manual impl: `ort::session::Session` and `tokenizers::Tokenizer` are
    // not `Debug`. Surface the public-facing fields so test assertions and
    // log macros can use `{:?}` on an `Embedder`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("dim", &self.dim)
            .field("max_length", &self.max_length)
            .finish_non_exhaustive()
    }
}

impl Embedder {
    /// Load the model and tokenizer from `model_dir`. The directory must
    /// contain both `model.onnx` and `tokenizer.json` (the canonical layout
    /// produced by `optimum-cli export onnx`).
    pub fn load(model_dir: impl AsRef<Path>) -> Result<Self> {
        let dir = model_dir.as_ref();
        let model_path = dir.join(MODEL_FILENAME);
        let tokenizer_path = dir.join(TOKENIZER_FILENAME);
        if !model_path.exists() {
            return Err(anyhow!("missing model file: {}", model_path.display()));
        }
        if !tokenizer_path.exists() {
            return Err(anyhow!(
                "missing tokenizer file: {}",
                tokenizer_path.display()
            ));
        }

        let tokenizer =
            load_tokenizer(&tokenizer_path).context("load BGE tokenizer from tokenizer.json")?;
        // ort 2.0.0-rc.10's `SessionBuilder` has no `commit_from_file` helper,
        // so we read the bytes ourselves and hand them to `commit_from_memory`.
        // BGE-small is ~130 MB on disk; loading it into RAM is fine for a
        // one-shot init and is what the underlying API would do anyway.
        let model_bytes = std::fs::read(&model_path)
            .with_context(|| format!("read ONNX model bytes from {}", model_path.display()))?;
        let session = Session::builder()
            .map_err(ort_err("create ort session builder"))?
            .commit_from_memory(&model_bytes)
            .map_err(ort_err(&format!(
                "load ONNX model from {}",
                model_path.display()
            )))?;

        Ok(Self {
            session,
            tokenizer,
            dim: EMBEDDING_DIM,
            max_length: MAX_SEQUENCE_LENGTH,
        })
    }

    /// Embedding dimensionality (384 for BGE-small).
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Embed a single string. The output is L2-normalized so cosine similarity
    /// against another embedding reduces to a dot product (which is the
    /// scoring function LanceDB uses internally for the `Cosine` metric).
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut batch = self.embed_batch(&[text])?;
        batch
            .pop()
            .ok_or_else(|| anyhow!("embedder returned no output for non-empty input"))
    }

    /// Embed a batch of strings in a single forward pass. Padding is dynamic
    /// (longest-in-batch); inputs longer than 512 tokens are truncated.
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let owned: Vec<String> = texts.iter().map(|t| (*t).to_string()).collect();
        let encodings = self
            .tokenizer
            .encode_batch(owned, /* add_special_tokens = */ true)
            .map_err(|e| anyhow!("tokenize batch: {e}"))?;

        let batch = encodings.len();
        // After padding, every encoding has the same length. Verify so we
        // can build a rectangular tensor without per-row indexing logic.
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);
        if seq_len == 0 {
            return Err(anyhow!("tokenizer produced empty encoding"));
        }
        if seq_len > self.max_length {
            return Err(anyhow!(
                "tokenizer returned {seq_len} tokens, exceeding configured max {}",
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
            // Padding guarantees ids.len() == seq_len for every row.
            for j in 0..seq_len {
                input_ids[[i, j]] = ids[j] as i64;
                attention_mask[[i, j]] = mask[j] as i64;
                token_type_ids[[i, j]] = types[j] as i64;
            }
        }

        // Build each input tensor explicitly: `Value::from_array` returns a
        // `Result<Value, ort::Error>` whose error type is not `StdError`, so
        // we cannot use `?` inside the `ort::inputs!` macro body.
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
            .map_err(ort_err("ort session run"))?;

        let hidden = outputs
            .get("last_hidden_state")
            .ok_or_else(|| anyhow!("model output missing last_hidden_state"))?;
        // ort rc.10's `try_extract_tensor` returns `(&Shape, &[T])`. Shape
        // derefs to `&[i64]`, giving the dimension extents.
        let (shape, data) = hidden
            .try_extract_tensor::<f32>()
            .map_err(ort_err("extract last_hidden_state as f32 tensor"))?;
        if shape.len() != 3 {
            return Err(anyhow!(
                "last_hidden_state has rank {}, expected 3",
                shape.len()
            ));
        }
        let seq = shape[1] as usize;
        let dim = shape[2] as usize;
        if dim != self.dim {
            return Err(anyhow!(
                "model hidden size {dim} does not match embedder dim {}",
                self.dim
            ));
        }
        let row_stride = seq * dim;

        let mut out = Vec::with_capacity(batch);
        for i in 0..batch {
            // CLS token sits at sequence position 0 of every batch row.
            let start = i * row_stride;
            let mut v: Vec<f32> = data[start..start + dim].to_vec();
            l2_normalize(&mut v);
            out.push(v);
        }
        Ok(out)
    }
}

fn load_tokenizer(path: &Path) -> Result<Tokenizer> {
    let mut t = Tokenizer::from_file(path)
        .map_err(|e| anyhow!("read tokenizer.json at {}: {e}", path.display()))?;
    // Configure padding so `encode_batch` returns rectangular tensors. The
    // tokenizer.json shipped with BGE-small *does* embed padding, but being
    // explicit insulates us from future model variants that drop it.
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
    .map_err(|e| anyhow!("configure truncation on BGE tokenizer: {e}"))?;
    Ok(t)
}

fn l2_normalize(v: &mut [f32]) {
    // Tiny epsilon prevents division-by-zero for degenerate all-zero rows
    // (which can happen on empty input after tokenization).
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denom = norm.max(1e-12);
    for x in v.iter_mut() {
        *x /= denom;
    }
}

/// Test helpers: download and cache the BGE-small ONNX assets so unit tests
/// can run against a real model. The download is one-shot per machine; the
/// cache lives under the OS temp dir so it never pollutes the repo or HOME.
///
/// Public for the integration-style tests in `vectors.rs` and `indexer.rs`
/// to reuse the same cached weights.
#[cfg(test)]
pub mod test_assets {
    use super::{MODEL_FILENAME, TOKENIZER_FILENAME};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    const HF_BASE: &str = "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main";
    /// The ONNX export of BGE-small lives under `onnx/model.onnx` on the
    /// hub. The tokenizer is at the repo root.
    const MODEL_URL: &str =
        "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/onnx/model.onnx";
    const TOKENIZER_URL: &str =
        "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/tokenizer.json";

    /// Read-write lock around the cache dir so concurrent test threads do not
    /// race on the partial-download file.
    static DOWNLOAD_LOCK: Mutex<()> = Mutex::new(());

    /// Returns the absolute cache directory. Reuses an existing model if both
    /// files are already present. Returns `Ok(None)` if the download cannot
    /// complete (no network, HuggingFace down) so callers can skip cleanly.
    pub fn ensure_model() -> Option<PathBuf> {
        let cache = std::env::var("LOCALMEM_TEST_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::temp_dir().join("localmem-test-models/bge-small-en-v1.5")
            });
        let _guard = DOWNLOAD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        if has_assets(&cache) {
            return Some(cache);
        }
        if std::fs::create_dir_all(&cache).is_err() {
            return None;
        }
        // Order: tokenizer (~1 MB) first as a smoke test, then the 130 MB
        // model. If either fails we bail and report skip.
        if !download(TOKENIZER_URL, &cache.join(TOKENIZER_FILENAME)) {
            return None;
        }
        if !download(MODEL_URL, &cache.join(MODEL_FILENAME)) {
            return None;
        }
        Some(cache)
    }

    /// Print a uniform "skip" message so a developer reading test output
    /// knows the embedder coverage was gated, not silently absent.
    pub fn skip_reason() -> String {
        format!(
            "skipping: could not download BGE-small-en-v1.5 assets. \
             Set LOCALMEM_TEST_MODEL_DIR to a directory containing \
             {MODEL_FILENAME} + {TOKENIZER_FILENAME} to run, or unblock \
             network access to {HF_BASE}"
        )
    }

    fn has_assets(dir: &Path) -> bool {
        dir.join(MODEL_FILENAME).exists() && dir.join(TOKENIZER_FILENAME).exists()
    }

    fn download(url: &str, dest: &Path) -> bool {
        // Two-attempt retry covers transient TLS / DNS hiccups on CI.
        for _ in 0..2 {
            if try_download_once(url, dest) {
                return true;
            }
        }
        false
    }

    fn try_download_once(url: &str, dest: &Path) -> bool {
        let tmp = dest.with_extension("partial");
        let Ok(resp) = ureq::get(url)
            .timeout(std::time::Duration::from_secs(120))
            .call()
        else {
            return false;
        };
        if resp.status() != 200 {
            return false;
        }
        let Ok(mut reader) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
        else {
            return false;
        };
        if std::io::copy(&mut resp.into_reader(), &mut reader).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        std::fs::rename(&tmp, dest).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_unit_norm() {
        let mut v = vec![3.0_f32, 4.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_zero_vector_does_not_panic() {
        let mut v = vec![0.0_f32; 4];
        l2_normalize(&mut v);
        // Result is all-zero (since denominator clamps to epsilon, all
        // numerators are 0). The important property is no NaN / panic.
        assert!(v.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn load_missing_dir_errors_clearly() {
        let err = Embedder::load(std::path::Path::new("/nonexistent/bge-model"))
            .expect_err("loading a missing model dir must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing"), "unexpected error: {msg}");
    }

    #[test]
    fn embed_returns_normalized_384_vec() {
        let Some(dir) = test_assets::ensure_model() else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let mut emb = Embedder::load(&dir).expect("load BGE-small");
        assert_eq!(emb.dim(), EMBEDDING_DIM);
        let v = emb.embed("hello world").expect("embed single string");
        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "expected unit norm, got {norm}");
    }

    #[test]
    fn embed_is_deterministic() {
        let Some(dir) = test_assets::ensure_model() else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let mut emb = Embedder::load(&dir).expect("load BGE-small");
        let a = emb
            .embed("Rust is a memory-safe systems language.")
            .unwrap();
        let b = emb
            .embed("Rust is a memory-safe systems language.")
            .unwrap();
        // Same input → same output, bit-for-bit (ONNX inference is
        // deterministic on CPU for the same model file).
        assert_eq!(a, b);
    }

    #[test]
    fn batch_matches_single_call() {
        let Some(dir) = test_assets::ensure_model() else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let mut emb = Embedder::load(&dir).expect("load BGE-small");
        let texts = ["alpha", "beta gamma delta", "the quick brown fox"];
        let batched = emb.embed_batch(&texts).unwrap();
        assert_eq!(batched.len(), 3);
        for v in &batched {
            assert_eq!(v.len(), EMBEDDING_DIM);
        }
        // Padding can shift attention-mask shapes, but the CLS token is
        // padding-invariant for BGE so individual embeds must match.
        for (i, t) in texts.iter().enumerate() {
            let single = emb.embed(t).unwrap();
            // Numerical jitter from differing batch shapes is bounded by
            // ONNX fp32 rounding (typ. < 1e-5 per dim).
            for (a, b) in single.iter().zip(batched[i].iter()) {
                assert!(
                    (a - b).abs() < 1e-4,
                    "single vs batch differ at idx {i} by more than 1e-4"
                );
            }
        }
    }

    #[test]
    fn batch_empty_returns_empty() {
        // Empty batch path should be cheap and not even touch the session,
        // so this test runs without needing the model on disk.
        let dir = std::env::temp_dir().join("localmem-test-bge-not-present");
        // Try the real model if available; otherwise we still exercise the
        // empty-batch shortcut via construction guard, but Embedder needs
        // the model to construct. So gate on availability.
        let Some(real) = test_assets::ensure_model() else {
            let _ = dir;
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let mut emb = Embedder::load(&real).expect("load BGE-small");
        let out = emb.embed_batch(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn embed_different_inputs_different_vectors() {
        let Some(dir) = test_assets::ensure_model() else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let mut emb = Embedder::load(&dir).expect("load BGE-small");
        let a = emb.embed("functional rust").unwrap();
        let b = emb.embed("rust programming language").unwrap();
        // Semantically related but not identical; cosine should be high but
        // strictly < 1.
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        assert!(dot < 0.999, "near-duplicate vectors for distinct prompts");
        assert!(dot > 0.5, "expected high similarity, got {dot}");
    }

    #[test]
    fn array_shape_helper_available() {
        // Smoke test that the ndarray dep is wired: we use Array2 in the
        // hot path; this guarantees a unit-level call site exists even when
        // the heavy integration tests are skipped on offline CI.
        let a = Array2::<f32>::zeros((2, 3));
        assert_eq!(a.shape(), &[2, 3]);
    }
}
