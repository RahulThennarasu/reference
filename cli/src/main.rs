use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;

use reference_core::embedding::Embedder;
use reference_core::paths;
use reference_core::store::Store;
use reference_core::synthesize;
use reference_core::watcher;

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
    /// Embed QUERY and return the closest indexed chunks.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 5)]
        top_k: usize,
        /// Emit machine-readable JSON instead of printed text — for
        /// scripts or agents (e.g. Claude Code shelling out via Bash)
        /// consuming results programmatically.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Serialize)]
struct JsonHit {
    path: String,
    start_line: i32,
    end_line: i32,
    chunk_kind: String,
    score: f32,
}

#[derive(Serialize)]
struct JsonCitation {
    path: String,
    snippet: String,
    start_line: usize,
    end_line: usize,
    chunk_kind: String,
}

#[derive(Serialize)]
struct JsonSearchResponse {
    results: Vec<JsonHit>,
    citations: Vec<JsonCitation>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Same index the Tauri app reads/writes (`~/.reference/index`), not a
    // separate one scoped to wherever this binary happens to be launched
    // from — that's what makes `reference-cli search` usable as a
    // general-purpose tool (e.g. from an agent's shell) against whatever
    // the app UI has already indexed.
    let db_uri = paths::default_db_uri();
    std::fs::create_dir_all(paths::default_app_data_dir())?;

    eprintln!("loading embedding model (all-MiniLM-L6-v2)...");
    let embedder = Embedder::load().await?;
    let store = Store::open(&db_uri).await?;

    match cli.command {
        Command::Watch => {
            let folder = PathBuf::from(WATCH_FOLDER);
            std::fs::create_dir_all(&folder)?;
            watcher::watch(&folder, &embedder, &store, Arc::new(AtomicBool::new(false))).await?;
        }
        Command::Search { query, top_k, json } => {
            let embedding = embedder.embed(&query)?;
            let hits = store.hybrid_search(&query, &embedding, top_k).await?;

            let citations = if synthesize::is_question(&query) && !hits.is_empty() {
                synthesize::synthesize(&embedder, &query, &hits)?
            } else {
                Vec::new()
            };

            if json {
                let response = JsonSearchResponse {
                    results: hits
                        .iter()
                        .map(|h| JsonHit {
                            path: h.path.clone(),
                            start_line: h.start_line,
                            end_line: h.end_line,
                            chunk_kind: h.chunk_kind.clone(),
                            score: h.score,
                        })
                        .collect(),
                    citations: citations
                        .iter()
                        .map(|c| JsonCitation {
                            path: c.path.clone(),
                            snippet: c.snippet.clone(),
                            start_line: c.start_line,
                            end_line: c.end_line,
                            chunk_kind: c.chunk_kind.clone(),
                        })
                        .collect(),
                };
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }

            if hits.is_empty() {
                println!("no results (index is empty — watch a folder from the app first)");
            }

            for citation in &citations {
                println!("{}", citation.snippet);
                println!("  source: {}:{}\n", citation.path, citation.start_line);
            }

            for hit in hits {
                println!(
                    "{:.4}  {}:{}-{} [{}]",
                    hit.score, hit.path, hit.start_line, hit.end_line, hit.chunk_kind
                );
            }
        }
    }

    Ok(())
}
