// Manual check for the embedding.rs truncation fix: confirms a chunk of text
// far longer than MiniLM's 512-token position-embedding limit no longer fails
// inside the BERT forward pass. Not wired into `cargo test` — run manually
// with `cargo run -p reference-core --example check_truncation`.
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

    // Also check a short, normal-sized input still works as a sanity baseline.
    let short = embedder.embed("a short query")?;
    assert_eq!(short.len(), EMBEDDING_DIM);
    println!("short input still embeds fine, {}-dim", short.len());

    println!("PASS: oversized input truncated instead of erroring");
    Ok(())
}
