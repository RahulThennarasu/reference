use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
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

/// Compiles every `.gitignore` found under `root` into a single matcher, so
/// live file events under `target/`, `node_modules/`, etc. can be skipped
/// the same way the initial scan skips them. Built once per `watch()` call;
/// a `.gitignore` added after startup won't retroactively apply.
fn build_gitignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == ".gitignore")
    {
        builder.add(entry.path());
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
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

/// Counts files under `folder` that would actually get indexed (respecting
/// `.gitignore`), stopping as soon as the count exceeds `cap`. Safe to call
/// on an enormous or accidental directory (a whole home folder, `/`, etc.)
/// because it only touches filesystem metadata and bails out early instead
/// of walking the full tree.
pub fn count_indexable_files(folder: &Path, cap: usize) -> usize {
    let mut count = 0;
    for _ in WalkBuilder::new(folder)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
    {
        count += 1;
        if count > cap {
            break;
        }
    }
    count
}

/// Indexes every existing text file under `folder`, then watches it for
/// create/modify events and keeps the index current. Files matched by any
/// `.gitignore` under `folder` (build output, dependencies, `.git`, etc.)
/// are skipped in both the initial scan and live updates. Returns once
/// `stop` is set to `true` from another thread, so callers can cancel an
/// in-progress watch (e.g. when the user un-watches a folder).
pub async fn watch(
    folder: &Path,
    embedder: &Embedder,
    store: &Store,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    println!("indexing existing files under {}", folder.display());
    for entry in WalkBuilder::new(folder)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
    {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Err(e) = index_file(embedder, store, entry.path()).await {
            eprintln!("failed to index {}: {e}", entry.path().display());
        }
    }

    let gitignore = build_gitignore(folder);

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(folder, RecursiveMode::Recursive)?;

    println!("watching {} for changes...", folder.display());
    loop {
        if stop.load(Ordering::Relaxed) {
            println!("stopped watching {}", folder.display());
            return Ok(());
        }

        let res = match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };

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
            if gitignore.matched_path_or_any_parents(path, false).is_ignore() {
                continue;
            }
            if let Err(e) = index_file(embedder, store, path).await {
                eprintln!("failed to index {}: {e}", path.display());
            }
        }
    }
}
