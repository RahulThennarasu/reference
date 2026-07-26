// Manual check for embedding.rs's truncation handling: confirms a chunk of
// text far longer than MiniLM's 512-token position-embedding limit no
// longer fails inside the BERT forward pass, and that
// `embed_batch_with_truncation` correctly flags it as truncated (gap #4,
// docs/feature-gaps.md — the flag is what lets the app surface a
// "truncated" badge instead of this being invisible). Not wired into
// `cargo test` — run manually with
// `cargo run -p reference-core --example check_truncation`.
use reference_core::embedding::{Embedder, EMBEDDING_DIM};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("loading embedding model...");
    let embedder = Embedder::load().await?;

    // ~2000 words, comfortably past the 512-token limit once tokenized.
    let oversized = "fn validate_jwt_token(token: &str) -> Result<Claims> { ".repeat(200);
    println!("input length: {} chars", oversized.len());

    let vec = embedder.embed(&oversized)?;
    println!("embedded ok: {}-dim vector, first values {:?}", vec.len(), &vec[..4]);
    assert_eq!(vec.len(), EMBEDDING_DIM);

    let [(_, oversized_truncated), (_, short_truncated)] = embedder
        .embed_batch_with_truncation(&[oversized, "a short query".to_string()])?
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected exactly 2 results"))?;
    println!("oversized input truncated flag: {oversized_truncated}");
    println!("short input truncated flag: {short_truncated}");
    assert!(oversized_truncated, "oversized input should be flagged as truncated");
    assert!(!short_truncated, "short input should not be flagged as truncated");

    println!("PASS: oversized input truncated instead of erroring, and correctly flagged");
    Ok(())
}
