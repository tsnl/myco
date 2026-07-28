//! MiniLM embedder via **Candle** (no ONNX Runtime / ort).
//!
//! sentence-transformers/all-MiniLM-L6-v2 assets are fetched on first use into
//! `~/.myco/models/` by [`crate::text_search::embed_assets`]; nothing is baked
//! into the binary. Once the cache is warm the embedder is fully offline.

use std::sync::{Mutex, OnceLock};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use super::embed_assets;

/// Hidden size for all-MiniLM-L6-v2.
pub const EMBED_DIM: usize = 384;
const MAX_SEQ_LEN: usize = 256;

struct CandleEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

/// Process-wide embedder (lazy). Failures are cached.
fn embedder() -> Result<std::sync::MutexGuard<'static, CandleEmbedder>, String> {
    static EMBEDDER: OnceLock<Result<Mutex<CandleEmbedder>, String>> = OnceLock::new();
    let slot = EMBEDDER.get_or_init(|| match load_minilm() {
        Ok(m) => Ok(Mutex::new(m)),
        Err(e) => Err(format!(
            "MiniLM (Candle) embedder init failed: {e}. \
             Assets are fetched on first use into ~/.myco/models/ (override with \
             MYCO_EMBED_CACHE); network is required until that cache is warm. \
             See src/text_search/embed_weights/README.md and `myco --help harness-ops`."
        )),
    });
    match slot {
        Ok(m) => m
            .lock()
            .map_err(|e| format!("MiniLM embedder mutex poisoned: {e}")),
        Err(e) => Err(e.clone()),
    }
}

/// Embed a single text; returns L2-normalized vector of length [`EMBED_DIM`].
pub fn embed_text(text: &str) -> Result<Vec<f32>, String> {
    let mut emb = embedder()?;
    emb.embed_one(text)
}

fn load_minilm() -> Result<CandleEmbedder, String> {
    let assets = embed_assets::load()?;

    let device = Device::Cpu;
    let config: Config =
        serde_json::from_slice(&assets.config).map_err(|e| format!("parse config.json: {e}"))?;

    let vb = VarBuilder::from_buffered_safetensors(assets.weights, DTYPE, &device)
        .map_err(|e| format!("safetensors: {e}"))?;
    let model = BertModel::load(vb, &config).map_err(|e| format!("BertModel::load: {e}"))?;

    let mut tokenizer =
        Tokenizer::from_bytes(&assets.tokenizer).map_err(|e| format!("tokenizer: {e}"))?;
    let _ = tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        ..Default::default()
    }));
    let _ = tokenizer.with_truncation(Some(TruncationParams {
        max_length: MAX_SEQ_LEN,
        ..Default::default()
    }));

    Ok(CandleEmbedder {
        model,
        tokenizer,
        device,
    })
}

impl CandleEmbedder {
    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| format!("tokenize: {e}"))?;
        let ids = encoding.get_ids();
        let mask = encoding.get_attention_mask();
        if ids.is_empty() {
            return Err("empty tokenization".into());
        }

        let token_ids = Tensor::new(ids, &self.device)
            .map_err(|e| format!("token_ids tensor: {e}"))?
            .unsqueeze(0)
            .map_err(|e| format!("unsqueeze: {e}"))?;
        let attention_mask = Tensor::new(mask, &self.device)
            .map_err(|e| format!("mask tensor: {e}"))?
            .unsqueeze(0)
            .map_err(|e| format!("unsqueeze mask: {e}"))?;
        let token_type_ids = token_ids
            .zeros_like()
            .map_err(|e| format!("token_type_ids: {e}"))?;

        let embeddings = self
            .model
            .forward(&token_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| format!("bert forward: {e}"))?;

        // Mean pool over tokens with attention mask (sentence-transformers style).
        let dtype = embeddings.dtype();
        let mask_f = attention_mask
            .to_dtype(dtype)
            .map_err(|e| format!("mask dtype: {e}"))?
            .unsqueeze(2)
            .map_err(|e| format!("mask unsqueeze: {e}"))?;
        let sum_mask = mask_f.sum(1).map_err(|e| format!("sum_mask: {e}"))?;
        let summed = embeddings
            .broadcast_mul(&mask_f)
            .map_err(|e| format!("mask mul: {e}"))?
            .sum(1)
            .map_err(|e| format!("sum: {e}"))?;
        let pooled = summed
            .broadcast_div(&sum_mask)
            .map_err(|e| format!("mean pool: {e}"))?;

        // L2 normalize
        let norm = pooled
            .sqr()
            .map_err(|e| e.to_string())?
            .sum_keepdim(1)
            .map_err(|e| e.to_string())?
            .sqrt()
            .map_err(|e| e.to_string())?;
        let normalized = pooled
            .broadcast_div(&norm)
            .map_err(|e| format!("l2: {e}"))?;

        let vec = normalized
            .squeeze(0)
            .map_err(|e| e.to_string())?
            .to_dtype(DType::F32)
            .map_err(|e| e.to_string())?
            .to_vec1::<f32>()
            .map_err(|e| format!("to_vec: {e}"))?;
        if vec.len() != EMBED_DIM {
            return Err(format!(
                "unexpected embed dim {} (want {EMBED_DIM})",
                vec.len()
            ));
        }
        Ok(vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_resolve() {
        let assets = embed_assets::load().expect("resolve MiniLM assets");
        assert!(
            assets.weights.len() > 1_000_000,
            "safetensors {} bytes",
            assets.weights.len()
        );
        assert!(assets.tokenizer.len() > 1000);
    }

    #[test]
    fn embedder_loads_and_embeds() {
        let v = embed_text("hello world").expect("embed");
        assert_eq!(v.len(), EMBED_DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "l2 norm {norm}");
    }

    #[test]
    fn similar_texts_closer_than_unrelated() {
        let a = embed_text("extract text from PDF documents").unwrap();
        let b = embed_text("parse PDF files and forms").unwrap();
        let c = embed_text("banana bread recipe with muffins").unwrap();
        let sim = |x: &[f32], y: &[f32]| -> f32 { x.iter().zip(y).map(|(u, v)| u * v).sum() };
        assert!(
            sim(&a, &b) > sim(&a, &c),
            "pdf-pdf {} vs pdf-banana {}",
            sim(&a, &b),
            sim(&a, &c)
        );
    }
}
