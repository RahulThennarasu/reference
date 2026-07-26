// Ad-hoc benchmark for `Store::hybrid_search` against a real on-disk index,
// to check the doc's flagged concern in docs/code-aware-chunking.md: does
// the full-table brute-force scan still hold up now that chunking means
// 5-20x more rows per file than before. Not wired into `cargo test` — run
// manually with `cargo run -p reference-core --example bench_hybrid_search
// -- <db_uri>`.
use std::time::Instant;

use reference_core::embedding::Embedder;
use reference_core::store::Store;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_uri = std::env::args().nth(1).unwrap_or_else(|| "../app/data/reference-index".to_string());

    println!("loading embedding model...");
    let embedder = Embedder::load().await?;
    let store = Store::open(&db_uri).await?;

    let row_count = store.row_count().await?;
    println!("indexed rows: {row_count}");

    let queries = [
        "function that validates JWT tokens",
        "how does dag visualization work",
        "hybrid search ranking",
        "watch folder for changes",
        "embed batch of texts",
    ];

    for q in queries {
        let embed_start = Instant::now();
        let embedding = embedder.embed(q)?;
        let embed_ms = embed_start.elapsed().as_secs_f64() * 1000.0;

        // Run a few times to smooth out first-call warmup noise.
        let mut search_ms = Vec::new();
        for _ in 0..5 {
            let search_start = Instant::now();
            let hits = store.hybrid_search(q, &embedding, 8, None).await?;
            search_ms.push(search_start.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(&hits);
        }
        let avg = search_ms.iter().sum::<f64>() / search_ms.len() as f64;
        let min = search_ms.iter().cloned().fold(f64::MAX, f64::min);
        let max = search_ms.iter().cloned().fold(f64::MIN, f64::max);

        println!(
            "{q:>45} | embed {embed_ms:6.1}ms | search avg {avg:6.2}ms (min {min:.2}, max {max:.2})"
        );
    }

    Ok(())
}
