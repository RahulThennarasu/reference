use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::{api::tokio::Api, Repo, RepoType};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
const REVISION: &str = "main";

pub const EMBEDDING_DIM: usize = 384;

pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    pub async fn load() -> Result<Self> {
        let device = crate::device::select()?;

        let api = Api::new().context("failed to create hf-hub api client")?;
        let repo = api.repo(Repo::with_revision(
            MODEL_ID.to_string(),
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
    /// 384-dim vector per input string.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

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
        Ok(out)
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_batch(&[text.to_string()])?.remove(0))
    }
}
