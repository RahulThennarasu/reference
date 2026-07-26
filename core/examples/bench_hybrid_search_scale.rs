// Synthetic scaling benchmark for `Store::hybrid_search`, complementing
// `bench_hybrid_search` (which only measures this project's own small
// index). Grows a scratch LanceDB table to sizes representative of a real
// chunked codebase (per docs/code-aware-chunking.md's "5-20x more rows than
// file count" estimate) and times search at each checkpoint. Not wired into
// `cargo test` — run manually with:
//   cargo run -p reference-core --example bench_hybrid_search_scale --release
use std::time::Instant;

use reference_core::store::{ChunkRecord, Store};

const EMBEDDING_DIM: usize = 384;
const CHECKPOINTS: &[usize] = &[1_000, 5_000, 20_000, 50_000];
const BATCH_SIZE: usize = 500;

/// Tiny deterministic PRNG (xorshift64) so the benchmark doesn't need to
/// pull in a `rand` dependency just for this one-off script.
struct Xorshift64(u64);
impl Xorshift64 {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() as f64 / u64::MAX as f64) as f32 * 2.0 - 1.0
    }
}

fn random_normalized_embedding(rng: &mut Xorshift64) -> Vec<f32> {
    let mut v: Vec<f32> = (0..EMBEDDING_DIM).map(|_| rng.next_f32()).collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in v.iter_mut() {
        *x /= norm;
    }
    v
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_dir = std::env::temp_dir().join(format!("reference-bench-scale-{}", std::process::id()));
    let db_uri = db_dir.to_string_lossy().to_string();
    let store = Store::open(&db_uri).await?;

    let mut rng = Xorshift64(0x2545F4914F6CDD1D);
    let mut inserted = 0usize;
    let mut batch_idx = 0usize;

    println!("{:>10}  {:>10}  {:>10}  {:>10}", "rows", "avg(ms)", "min(ms)", "max(ms)");

    for &target in CHECKPOINTS {
        while inserted < target {
            let n = BATCH_SIZE.min(target - inserted);
            let chunks: Vec<ChunkRecord> = (0..n)
                .map(|i| ChunkRecord {
                    start_line: i as i32 + 1,
                    end_line: i as i32 + 3,
                    kind: "function".to_string(),
                    content: format!("fn synthetic_{batch_idx}_{i}() {{ /* bench filler */ }}"),
                    embedding: random_normalized_embedding(&mut rng),
                })
                .collect();
            store
                .replace_chunks(&format!("synthetic/file_{batch_idx}.rs"), chunks)
                .await?;
            inserted += n;
            batch_idx += 1;
        }

        let row_count = store.row_count().await?;
        let query_embedding = random_normalized_embedding(&mut rng);

        let mut timings = Vec::new();
        for _ in 0..8 {
            let start = Instant::now();
            let hits = store.hybrid_search("synthetic query text", &query_embedding, 8, None).await?;
            timings.push(start.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(&hits);
        }
        let avg = timings.iter().sum::<f64>() / timings.len() as f64;
        let min = timings.iter().cloned().fold(f64::MAX, f64::min);
        let max = timings.iter().cloned().fold(f64::MIN, f64::max);
        println!("{row_count:>10}  {avg:>10.2}  {min:>10.2}  {max:>10.2}");
    }

    std::fs::remove_dir_all(&db_dir).ok();
    Ok(())
}
