use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use walkdir::WalkDir;

use crate::chunk;
use crate::embedding::Embedder;
use crate::store::{ChunkRecord, Store};

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

    // Canonicalize so the same file always upserts under the same key,
    // regardless of whether it was reached via the initial walk (relative
    // to the watch root) or a notify event (already absolute).
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path_str = canonical.to_string_lossy().to_string();

    // Parse into function/impl-level chunks when a chunker exists for this
    // extension; a missing chunker or a parse failure (syntax error in a
    // WIP file, binary misdetected as text) falls back to one whole-file
    // chunk rather than dropping the file from the index.
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let chunks = chunk::chunk_or_whole_file(extension, &content);
    let chunk_count = chunks.len();

    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    // `_with_truncation`, not plain `embed_batch`: this is the indexing
    // path, where a chunk silently getting cut off means part of it never
    // becomes searchable — see gap #4 in docs/feature-gaps.md. The bool
    // rides along into `ChunkRecord`/the `truncated` column so the app can
    // surface it on affected results instead of it being invisible.
    let embeddings = embedder.embed_batch_with_truncation(&texts)?;
    let truncated_count = embeddings.iter().filter(|(_, t)| *t).count();

    let records: Vec<ChunkRecord> = chunks
        .into_iter()
        .zip(embeddings)
        .map(|(c, (embedding, truncated))| ChunkRecord {
            start_line: c.start_line,
            end_line: c.end_line,
            kind: c.kind,
            content: c.content,
            embedding,
            name: c.name.unwrap_or_default(),
            truncated,
        })
        .collect();

    store.replace_chunks(&path_str, records).await?;
    println!("indexed {path_str} ({chunk_count} chunk(s))");
    if truncated_count > 0 {
        println!(
            "  warning: {truncated_count} chunk(s) in {path_str} exceeded the embedding model's token limit and were truncated — search may miss content near the end of those chunks"
        );
    }
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
        // `ignore::WalkBuilder` only honors `.gitignore` when `folder` is
        // inside an actual git repo by default (`require_git` defaults to
        // true) — watched folders here are arbitrary user-chosen
        // directories, not necessarily git repos, so without this a
        // `.gitignore` in a non-git folder is silently ignored.
        .require_git(false)
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
        // See `count_indexable_files`'s comment: without this, `.gitignore`
        // is silently skipped for any watched folder that isn't itself a
        // git repo.
        .require_git(false)
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

// Regression tests for the live watch/index pipeline — the piece that runs
// continuously in a shipped app, where a broken `.gitignore` filter or a
// `stop` flag that doesn't actually stop the loop fails silently (no crash,
// just files that quietly stop getting indexed). Uses the real candle model,
// same rationale as core/src/store.rs's tests: stubbed embeddings wouldn't
// catch a real model-loading regression.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::EmbeddingModel;
    use tokio::sync::OnceCell;

    static EMBEDDER: OnceCell<Embedder> = OnceCell::const_new();

    async fn embedder() -> &'static Embedder {
        EMBEDDER
            .get_or_init(|| async { Embedder::load(EmbeddingModel::MiniLmL6).await.unwrap() })
            .await
    }

    async fn open_temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().to_str().unwrap()).await.expect("open store");
        (dir, store)
    }

    async fn wait_until<F, Fut>(mut cond: F, timeout: Duration) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let start = std::time::Instant::now();
        loop {
            if cond().await {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Runs `watch()` on its own dedicated OS thread with its own fresh
    /// `tokio::Runtime`, exactly mirroring how `app/src-tauri/src/lib.rs`
    /// actually spawns it in production (`std::thread::spawn` + `rt.block_on`,
    /// not `tokio::spawn`). This matters: `watch()`'s live loop blocks its
    /// thread synchronously on `mpsc::Receiver::recv_timeout`, which is not
    /// an `.await` point — `tokio::spawn`-ing it directly onto a test's
    /// (single-threaded-by-default) runtime starves every other task on
    /// that runtime, including the one that would set `stop`, and hangs
    /// forever. Production never hits this because it always gets its own
    /// thread.
    fn spawn_watch_thread(
        embedder: &'static Embedder,
        store: Arc<Store>,
        path: std::path::PathBuf,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Result<()>> {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("watch test runtime");
            rt.block_on(watch(&path, embedder, &store, stop))
        })
    }

    async fn join_with_timeout(handle: std::thread::JoinHandle<Result<()>>, timeout: Duration) {
        let joined = tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || handle.join()))
            .await
            .expect("watch thread must join promptly once `stop` is set, not hang")
            .expect("join task itself must not panic");
        match joined {
            Ok(result) => assert!(result.is_ok(), "watch() must return Ok: {:?}", result.err()),
            Err(_) => panic!("watch() thread panicked"),
        }
    }

    #[test]
    fn count_indexable_files_stops_early_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "hello").unwrap();
        }
        // Cap of 3 means the walk should bail as soon as it sees a 4th file,
        // not walk all 10 — this is what makes it safe to call on an
        // enormous accidental directory (a whole home folder, `/`).
        assert_eq!(count_indexable_files(dir.path(), 3), 4);
        assert_eq!(count_indexable_files(dir.path(), 100), 10);
    }

    #[test]
    fn build_gitignore_matches_files_listed_in_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "secret").unwrap();
        std::fs::write(dir.path().join("kept.txt"), "public").unwrap();

        let gitignore = build_gitignore(dir.path());
        assert!(gitignore
            .matched_path_or_any_parents(dir.path().join("ignored.txt"), false)
            .is_ignore());
        assert!(!gitignore
            .matched_path_or_any_parents(dir.path().join("kept.txt"), false)
            .is_ignore());
    }

    #[tokio::test]
    async fn watch_indexes_existing_files_then_stops_promptly_on_signal() {
        let embedder = embedder().await;
        let (store_dir, store) = open_temp_store().await;
        let store = Arc::new(store);

        let watch_dir = tempfile::tempdir().unwrap();
        std::fs::write(watch_dir.path().join("a.txt"), "content about widgets").unwrap();
        std::fs::write(watch_dir.path().join("b.txt"), "content about gadgets").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_watch_thread(embedder, store.clone(), watch_dir.path().to_path_buf(), stop.clone());

        assert!(
            wait_until(|| async { store.row_count().await.unwrap() >= 2 }, Duration::from_secs(15)).await,
            "both pre-existing files should get indexed by the initial scan"
        );

        stop.store(true, Ordering::Relaxed);
        join_with_timeout(handle, Duration::from_secs(5)).await;

        drop(store_dir);
    }

    #[tokio::test]
    async fn watch_skips_gitignored_files_in_the_initial_scan() {
        let embedder = embedder().await;
        let (_store_dir, store) = open_temp_store().await;
        let store = Arc::new(store);

        let watch_dir = tempfile::tempdir().unwrap();
        std::fs::write(watch_dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(watch_dir.path().join("ignored.txt"), "content about secrets").unwrap();
        std::fs::write(watch_dir.path().join("kept.txt"), "content about widgets").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_watch_thread(embedder, store.clone(), watch_dir.path().to_path_buf(), stop.clone());

        assert!(
            wait_until(|| async { store.row_count().await.unwrap() >= 1 }, Duration::from_secs(15)).await,
            "the non-ignored file should get indexed"
        );
        // Give the (would-be) ignored file every chance to show up before
        // asserting it never does.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(store.row_count().await.unwrap(), 1, "ignored.txt must not be indexed");

        let query_embedding = embedder.embed("widgets").unwrap();
        let hits = store
            .hybrid_search("widgets", &query_embedding, 10, None, &crate::store::RankingWeights::default())
            .await
            .unwrap();
        assert!(hits.iter().all(|h| !h.path.ends_with("ignored.txt")));

        stop.store(true, Ordering::Relaxed);
        join_with_timeout(handle, Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn watch_indexes_files_created_after_the_initial_scan() {
        let embedder = embedder().await;
        let (_store_dir, store) = open_temp_store().await;
        let store = Arc::new(store);

        // Empty folder: the initial scan finishes near-instantly, so this
        // exercises the live notify-event path, not the initial walk.
        let watch_dir = tempfile::tempdir().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_watch_thread(embedder, store.clone(), watch_dir.path().to_path_buf(), stop.clone());

        // Give the watcher time to move past the initial scan and register
        // its OS-level watch before the file shows up.
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::write(watch_dir.path().join("new.txt"), "content about live updates").unwrap();

        assert!(
            wait_until(|| async { store.row_count().await.unwrap() >= 1 }, Duration::from_secs(15)).await,
            "a file created while watching should be indexed via the live notify event, not just the initial scan"
        );

        stop.store(true, Ordering::Relaxed);
        join_with_timeout(handle, Duration::from_secs(5)).await;
    }
}
