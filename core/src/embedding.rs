use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::{api::tokio::Api, Repo, RepoType};
use serde::{Deserialize, Serialize};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

const REVISION: &str = "main";

/// User-selectable embedding models — gap #5 in docs/feature-gaps.md,
/// scoped down to just this axis of "zero configurability" (not gpu
/// backend or ranking weights, see that doc for why). Deliberately
/// restricted to models that are both (a) 384-dim, matching this project's
/// `EMBEDDING_DIM` — a different dimension would mean changing the LanceDB
/// schema's `FixedSizeList` width, not just reindexing, a much bigger
/// change — and (b) BERT-architecture, since `candle_transformers` 0.11.0's
/// `bert` module (confirmed via its `Config`, which derives `Deserialize`
/// straight from each model's own `config.json`) only covers that family;
/// a popular non-BERT model like `all-mpnet-base-v2` (MPNet) isn't loadable
/// through this code path at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EmbeddingModel {
    #[default]
    MiniLmL6,
    MiniLmL12,
    BgeSmall,
    GteSmall,
}

impl EmbeddingModel {
    pub fn repo_id(&self) -> &'static str {
        match self {
            EmbeddingModel::MiniLmL6 => "sentence-transformers/all-MiniLM-L6-v2",
            EmbeddingModel::MiniLmL12 => "sentence-transformers/all-MiniLM-L12-v2",
            EmbeddingModel::BgeSmall => "BAAI/bge-small-en-v1.5",
            EmbeddingModel::GteSmall => "thenlper/gte-small",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            EmbeddingModel::MiniLmL6 => "MiniLM-L6 (fast, default)",
            EmbeddingModel::MiniLmL12 => "MiniLM-L12 (slower, more accurate)",
            EmbeddingModel::BgeSmall => "BGE-small (retrieval-tuned)",
            EmbeddingModel::GteSmall => "GTE-small (retrieval-tuned)",
        }
    }
}

/// Reads just the `embedding_model` field out of the app's settings file
/// (see `app/src-tauri/src/lib.rs`'s `AppSettings`), ignoring every other
/// field (`ranking_weights` etc.) rather than needing to know that whole
/// shape here. This exists for the MCP server: it opens the exact same
/// on-disk index the app writes to, so if the app has switched embedding
/// models, MCP's query embeddings have to come from that same model too —
/// cosine similarity between vectors from two different models is
/// meaningless, so silently defaulting here would just make MCP search
/// return garbage-ranked results the moment the app's choice diverges from
/// the default. Falls back to the default on any read/parse failure (no
/// settings file yet, fresh install) same as the app does.
pub fn load_configured_model(settings_path: &std::path::Path) -> EmbeddingModel {
    #[derive(Deserialize, Default)]
    struct Partial {
        #[serde(default)]
        embedding_model: EmbeddingModel,
    }
    std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Partial>(&s).ok())
        .unwrap_or_default()
        .embedding_model
}

pub const EMBEDDING_DIM: usize = 384;

pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    pub async fn load(model: EmbeddingModel) -> Result<Self> {
        let device = crate::device::select()?;

        let api = Api::new().context("failed to create hf-hub api client")?;
        let repo = api.repo(Repo::with_revision(
            model.repo_id().to_string(),
            RepoType::Model,
            REVISION.to_string(),
        ));

        let config_path = repo.get("config.json").await?;
        let tokenizer_path = repo.get("tokenizer.json").await?;
        let weights_path = repo.get("model.safetensors").await?;

        let config: Config = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;

        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));
        // Without this, a chunk longer than the model's position-embedding limit
        // (whole-file fallback, or an unusually large function) fails deep inside
        // the BERT forward pass instead of being truncated up front.
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: config.max_position_embeddings,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("failed to set tokenizer truncation: {e}"))?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)?
        };
        let model = BertModel::load(vb, &config)?;

        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embeds a batch of texts, returning one mean-pooled, L2-normalized
    /// 384-dim vector per input string. Discards per-input truncation info —
    /// use `embed_batch_with_truncation` when that matters (indexing, where
    /// silently truncating a chunk means part of it never becomes
    /// searchable — not query embedding, where queries are always short).
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(self
            .embed_batch_with_truncation(texts)?
            .into_iter()
            .map(|(v, _)| v)
            .collect())
    }

    /// Same as `embed_batch`, but also reports whether each input exceeded
    /// the tokenizer's `max_length` (set in `load()` from the model's
    /// `max_position_embeddings`) and got silently cut down to fit.
    /// `Tokenizer::truncate` moves anything past that limit into the
    /// encoding's `overflowing` list rather than erroring — checking
    /// whether that list is non-empty is the cheap, reliable way to detect
    /// it happened, no separate un-truncated tokenization pass needed.
    pub fn embed_batch_with_truncation(&self, texts: &[String]) -> Result<Vec<(Vec<f32>, bool)>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
        let truncated: Vec<bool> = encodings.iter().map(|e| !e.get_overflowing().is_empty()).collect();

        let token_ids: Vec<Tensor> = encodings
            .iter()
            .map(|enc| {
                let ids = enc.get_ids().to_vec();
                Tensor::new(ids.as_slice(), &self.device)
            })
            .collect::<candle_core::Result<_>>()?;
        let attention_mask: Vec<Tensor> = encodings
            .iter()
            .map(|enc| {
                let mask = enc.get_attention_mask().to_vec();
                Tensor::new(mask.as_slice(), &self.device)
            })
            .collect::<candle_core::Result<_>>()?;

        let token_ids = Tensor::stack(&token_ids, 0)?;
        let attention_mask = Tensor::stack(&attention_mask, 0)?;
        let token_type_ids = token_ids.zeros_like()?;

        let embeddings =
            self.model
                .forward(&token_ids, &token_type_ids, Some(&attention_mask))?;

        // Mean pooling over the sequence dimension, respecting the attention mask.
        let mask = attention_mask.to_dtype(DType::F32)?;
        let mask_expanded = mask.unsqueeze(2)?.broadcast_as(embeddings.shape())?;
        let summed = (embeddings * &mask_expanded)?.sum(1)?;
        let counts = mask_expanded.sum(1)?;
        let pooled = summed.broadcast_div(&counts)?;

        // L2 normalize each row so cosine similarity reduces to a dot product.
        let norm = pooled.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = pooled.broadcast_div(&norm)?;

        let out: Vec<Vec<f32>> = normalized.to_vec2()?;
        Ok(out.into_iter().zip(truncated).collect())
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_batch(&[text.to_string()])?.remove(0))
    }
}
