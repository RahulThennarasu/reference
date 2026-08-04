use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use walkdir::WalkDir;

use serde::Serialize;

use crate::chunk;
use crate::embedding::Embedder;
use crate::store::{ChunkRecord, Store};

/// Caps how many failed paths `IndexProgress::failed` carries so a folder
/// with hundreds of unreadable files doesn't balloon every progress tick's
/// payload — `failed_count` still tracks the true total separately, so the
/// UI can show "N failed" even once the list itself is capped.
const MAX_TRACKED_FAILURES: usize = 20;

/// Snapshot of how far the initial scan of a watched folder has gotten.
/// `done` is the signal a progress bar should key off of to know indexing
/// actually finished, not `indexed == total`: the file count used for
/// `total` is taken before the scan starts (`count_indexable_files`), so a
/// file created/deleted mid-scan can make `indexed` over/undershoot `total`
/// slightly — `done` is set exactly once, after the scan loop truly exits.
/// `failed`/`failed_count` exist because a file that errors out of
/// `index_file` previously only ever got an `eprintln!` — invisible in a
/// packaged `.app` with no attached terminal, so a folder could finish
/// scanning with files silently missing from the index and nothing in the
/// UI showing it.
#[derive(Debug, Clone, Serialize)]
pub struct IndexProgress {
    pub indexed: usize,
    pub total: usize,
    pub done: bool,
    pub failed_count: usize,
    pub failed: Vec<String>,
}

/// Reads a file as UTF-8 text. Returns `None` for files that aren't valid
/// text (binaries, etc.) so they're silently skipped rather than indexed
/// with garbage content.
fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Lockfiles: machine-generated, no chunker exists for their formats (TOML/
/// YAML/etc. aren't in `chunk`'s language list), so they fall back to one
/// whole-file chunk — and being generated, that one chunk can be enormous
/// (`Cargo.lock` alone was observed at ~266KB of content in a single search
/// result, blowing past a caller's token budget). Checked in, not
/// `.gitignore`d, so gitignore filtering alone doesn't skip them.
const SKIPPED_FILENAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "poetry.lock",
    "Pipfile.lock",
    "composer.lock",
    "Gemfile.lock",
    "go.sum",
];

fn is_skipped_file(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| SKIPPED_FILENAMES.contains(&n))
}

/// How often the periodic full re-scan re-walks a watched folder as a
/// self-healing fallback for the live `notify` watch — FSEvents delivery on
/// macOS has been observed to be non-deterministic under system load
/// (sometimes instant, sometimes delayed well past what a live watch alone
/// should need), so this exists purely to catch whatever a missed/delayed
/// live event didn't. Cheap by construction: `last_mtime` means every file
/// whose on-disk mtime hasn't changed since it was last embedded costs one
/// `fs::metadata` stat, not a re-embed.
const PERIODIC_RESCAN_INTERVAL: Duration = Duration::from_secs(300);

/// Unix epoch seconds for `path`'s on-disk mtime, or `None` if it can't be
/// read (deleted mid-scan, permissions, etc.) — callers treat that the same
/// as "unknown," i.e. always worth (re-)indexing rather than erroring out.
fn file_mtime(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(secs as i64)
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

/// Indexes `path`, embedding and writing chunks via `store.replace_chunks`.
/// `mtime` is the on-disk mtime the caller already read (Unix epoch
/// seconds) — passed in rather than re-read here so a caller like the
/// periodic re-scan can decide *not* to call this at all when the mtime
/// it already has matches what's cached, without a redundant stat inside.
async fn index_file(embedder: &Embedder, store: &Store, path: &Path, mtime: i64) -> Result<()> {
    if is_skipped_file(path) {
        return Ok(());
    }
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
            mtime,
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
        .filter(|e| !is_skipped_file(e.path()))
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
    total_files: usize,
    on_progress: impl Fn(IndexProgress) + Send + Sync + 'static,
) -> Result<()> {
    // Registered before the initial scan (not after) so an edit made *during*
    // a long initial scan of a large folder isn't lost forever — notify only
    // delivers events from the moment `.watch()` is called, and a big real
    // project's initial scan can take long enough for that window to matter.
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(folder, RecursiveMode::Recursive)?;

    // Tracks the on-disk mtime last successfully embedded for each path —
    // updated by the initial scan, live events, and the periodic re-scan
    // alike. The periodic re-scan (below) consults this to skip a file
    // whose mtime hasn't moved since it was last indexed, so self-healing
    // against a missed/delayed live event costs a stat per file, not a
    // re-embed of the whole folder every interval.
    let mut last_mtime: HashMap<String, i64> = HashMap::new();

    println!("indexing existing files under {}", folder.display());
    on_progress(IndexProgress { indexed: 0, total: total_files, done: false, failed_count: 0, failed: Vec::new() });
    let mut indexed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut failed_count = 0usize;
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
        let mtime = file_mtime(entry.path()).unwrap_or(0);
        if let Err(e) = index_file(embedder, store, entry.path(), mtime).await {
            eprintln!("failed to index {}: {e}", entry.path().display());
            failed_count += 1;
            if failed.len() < MAX_TRACKED_FAILURES {
                failed.push(entry.path().display().to_string());
            }
        } else {
            last_mtime.insert(entry.path().to_string_lossy().to_string(), mtime);
        }
        indexed += 1;
        // Clamped: `total_files` is counted before the walk starts, so a
        // file created mid-scan can otherwise push `indexed` past it and
        // report a progress bar over 100%.
        on_progress(IndexProgress {
            indexed: indexed.min(total_files),
            total: total_files,
            done: false,
            failed_count,
            failed: failed.clone(),
        });
    }

    // `done: true` is the actual completion signal (see `IndexProgress`'s
    // doc comment) — fired once, after the scan loop has fully exited,
    // regardless of whether `indexed` landed exactly on `total_files`.
    on_progress(IndexProgress { indexed: total_files, total: total_files, done: true, failed_count, failed });

    let gitignore = build_gitignore(folder);
    let mut last_full_rescan = Instant::now();

    println!("watching {} for changes...", folder.display());
    loop {
        if stop.load(Ordering::Relaxed) {
            println!("stopped watching {}", folder.display());
            return Ok(());
        }

        let res = match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Self-healing fallback for `notify` delivery that's missed
                // or arrived much later than expected (observed in practice
                // to be non-deterministic under system load on macOS) — see
                // `PERIODIC_RESCAN_INTERVAL`'s doc comment. Only reachable on
                // an idle tick (no pending fs event), so it never competes
                // with actually-arriving live events for this thread's time.
                if last_full_rescan.elapsed() >= PERIODIC_RESCAN_INTERVAL {
                    last_full_rescan = Instant::now();
                    for entry in WalkBuilder::new(folder)
                        .require_git(false)
                        .build()
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                    {
                        if stop.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        let path = entry.path();
                        if gitignore.matched_path_or_any_parents(path, false).is_ignore() {
                            continue;
                        }
                        let Some(mtime) = file_mtime(path) else { continue };
                        let path_key = path.to_string_lossy().to_string();
                        if last_mtime.get(&path_key) == Some(&mtime) {
                            continue;
                        }
                        if let Err(e) = index_file(embedder, store, path, mtime).await {
                            eprintln!("periodic re-scan failed to index {}: {e}", path.display());
                        } else {
                            last_mtime.insert(path_key, mtime);
                        }
                    }
                }
                continue;
            }
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
            let mtime = file_mtime(path).unwrap_or(0);
            let path_key = path.to_string_lossy().to_string();
            // A single save can fire several distinct notify events for the
            // same path (editors commonly touch metadata separately from
            // content, or emit both a Create and a Modify for one write) —
            // without this, one `Cmd+S` re-embeds the same unchanged content
            // several times in a row. Safe to skip on an exact mtime match:
            // this is the live path, so the file's content genuinely hasn't
            // changed since the last time this same mtime was indexed.
            if last_mtime.get(&path_key) == Some(&mtime) {
                continue;
            }
            if let Err(e) = index_file(embedder, store, path, mtime).await {
                eprintln!("failed to index {}: {e}", path.display());
            } else {
                last_mtime.insert(path_key, mtime);
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
            let total = count_indexable_files(&path, usize::MAX);
            rt.block_on(watch(&path, embedder, &store, stop, total, |_| {}))
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
            .hybrid_search("widgets", &query_embedding, 10, None, None, &crate::store::RankingWeights::default())
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
