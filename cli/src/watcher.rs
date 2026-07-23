use std::path::Path;
use std::sync::mpsc;

use anyhow::Result;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use walkdir::WalkDir;

use crate::embedding::Embedder;
use crate::store::Store;

/// Reads a file as UTF-8 text. Returns `None` for files that aren't valid
/// text (binaries, etc.) so they're silently skipped rather than indexed
/// with garbage content.
fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

async fn index_file(embedder: &Embedder, store: &Store, path: &Path) -> Result<()> {
    let Some(content) = read_text(path) else {
        return Ok(());
    };
    if content.trim().is_empty() {
        return Ok(());
    }

    let embedding = embedder.embed(&content)?;
    // Canonicalize so the same file always upserts under the same key,
    // regardless of whether it was reached via the initial walk (relative
    // to the watch root) or a notify event (already absolute).
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path_str = canonical.to_string_lossy().to_string();
    store.upsert(&path_str, &content, embedding).await?;
    println!("indexed {path_str}");
    Ok(())
}

/// Indexes every existing text file under `folder`, then watches it for
/// create/modify events and keeps the index current.
pub async fn watch(folder: &Path, embedder: &Embedder, store: &Store) -> Result<()> {
    println!("indexing existing files under {}", folder.display());
    for entry in WalkDir::new(folder)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        if let Err(e) = index_file(embedder, store, entry.path()).await {
            eprintln!("failed to index {}: {e}", entry.path().display());
        }
    }

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(folder, RecursiveMode::Recursive)?;

    println!("watching {} for changes...", folder.display());
    for res in rx {
        let event = match res {
            Ok(event) => event,
            Err(e) => {
                eprintln!("watch error: {e}");
                continue;
            }
        };

        let is_relevant = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
        if !is_relevant {
            continue;
        }

        for path in &event.paths {
            if !path.is_file() {
                continue;
            }
            if let Err(e) = index_file(embedder, store, path).await {
                eprintln!("failed to index {}: {e}", path.display());
            }
        }
    }

    Ok(())
}
