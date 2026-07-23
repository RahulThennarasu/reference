use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use reference_core::embedding::Embedder;
use reference_core::store::Store;
use reference_core::synthesize;
use reference_core::watcher;

const DB_URI: &str = "data/reference-index";

// Hardcoded folder for this first vertical slice; folder selection becomes
// a real UI/config option once the Tauri shell exists.
const WATCH_FOLDER: &str = "./watched";

#[derive(Parser)]
#[command(name = "reference-cli")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Watch WATCH_FOLDER, embedding and indexing new/changed files.
    Watch,
    /// Embed QUERY and return the closest indexed files.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 5)]
        top_k: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    println!("loading embedding model (all-MiniLM-L6-v2)...");
    let embedder = Embedder::load().await?;
    let store = Store::open(DB_URI).await?;

    match cli.command {
        Command::Watch => {
            let folder = PathBuf::from(WATCH_FOLDER);
            std::fs::create_dir_all(&folder)?;
            watcher::watch(&folder, &embedder, &store).await?;
        }
        Command::Search { query, top_k } => {
            let embedding = embedder.embed(&query)?;
            let hits = store.hybrid_search(&query, &embedding, top_k).await?;
            if hits.is_empty() {
                println!("no results (index is empty — run `watch` first)");
            }

            if synthesize::is_question(&query) && !hits.is_empty() {
                let answer = synthesize::synthesize(&embedder, &query, &hits)?;
                println!("{}\n", answer.summary);
                for citation in &answer.citations {
                    println!("  source: {}", citation.path);
                }
                println!();
            }

            for hit in hits {
                println!("{:.4}  {}", hit.score, hit.path);
            }
        }
    }

    Ok(())
}
