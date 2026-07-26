use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use reference_core::embedding::Embedder;
use reference_core::paths;
use reference_core::store::Store;
use reference_core::synthesize;
use reference_core::watcher;
use serde::Serialize;
use tauri::State;

/// The stop flag alone isn't enough to safely purge a folder's indexed rows:
/// `watcher::watch`'s loop only checks it *before* waiting for the next
/// filesystem event, so an event already queued when the flag flips still
/// gets fully processed (including an `upsert`) after the flag is set. The
/// join handle lets `stop_watch` actually wait for the thread to exit —
/// past its last possible upsert — before running `delete_under`, instead
/// of racing a delete against a straggling write.
struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

struct AppState {
    embedder: Arc<Embedder>,
    store: Arc<Store>,
    // Each entry gets its own background watch thread; re-adding an
    // already-watched folder is a no-op instead of spawning a duplicate.
    watching: Mutex<HashMap<PathBuf, WatchHandle>>,
}

fn load_watched_folders() -> Vec<String> {
    std::fs::read_to_string(paths::default_watched_folders_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_watched_folders(state: &AppState) {
    let folders: Vec<String> = match state.watching.lock() {
        Ok(watching) => watching.keys().map(|p| p.to_string_lossy().to_string()).collect(),
        Err(e) => {
            eprintln!("failed to lock watching state for persistence: {e}");
            return;
        }
    };
    match serde_json::to_string(&folders) {
        Ok(json) => {
            if let Err(e) = std::fs::write(paths::default_watched_folders_path(), json) {
                eprintln!("failed to persist watched folders: {e}");
            }
        }
        Err(e) => eprintln!("failed to serialize watched folders: {e}"),
    }
}

#[derive(Serialize)]
struct SearchResult {
    path: String,
    score: f32,
    start_line: usize,
    end_line: usize,
    chunk_kind: String,
}

#[derive(Serialize)]
struct Citation {
    path: String,
    snippet: String,
    start_line: usize,
    end_line: usize,
    chunk_kind: String,
}

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
    // Each citation stands on its own (snippet + source file) — deliberately
    // not collapsed into one joined paragraph, since concatenating snippets
    // pulled from unrelated files reads as an incoherent run-on sentence.
    citations: Vec<Citation>,
}

// Answer synthesis needs a much wider candidate pool than the results list
// shown in the UI (`top_k`, typically 8) — it filters down to prose files
// internally, and with enough indexed folders/noise in the mix, the actual
// relevant .md/.txt files can easily rank outside the top 8 overall while
// still being the best prose match. hybrid_search already scores the whole
// table before truncating, so asking for more rows here costs nothing extra.
const SYNTHESIS_CANDIDATE_POOL: usize = 50;

#[tauri::command]
async fn search(
    state: State<'_, AppState>,
    query: String,
    top_k: usize,
) -> Result<SearchResponse, String> {
    let embedding = state.embedder.embed(&query).map_err(|e| e.to_string())?;
    let hits = state
        .store
        .hybrid_search(&query, &embedding, top_k.max(SYNTHESIS_CANDIDATE_POOL))
        .await
        .map_err(|e| e.to_string())?;

    let citations = if synthesize::is_question(&query) && !hits.is_empty() {
        synthesize::synthesize(&state.embedder, &query, &hits)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|c| Citation {
                path: c.path,
                snippet: c.snippet,
                start_line: c.start_line,
                end_line: c.end_line,
                chunk_kind: c.chunk_kind,
            })
            .collect()
    } else {
        Vec::new()
    };

    let results = hits
        .into_iter()
        .take(top_k)
        .map(|h| SearchResult {
            path: h.path,
            score: h.score,
            start_line: h.start_line as usize,
            end_line: h.end_line as usize,
            chunk_kind: h.chunk_kind,
        })
        .collect();

    Ok(SearchResponse { results, citations })
}

/// Finds the most relevant line in `path` for `query`, so a plain search
/// result (which — unlike a citation — doesn't already have a known
/// snippet) can still be opened at the right spot instead of line 1.
/// Re-reads the file from disk rather than the indexed copy, so it reflects
/// the file's current contents even if it's changed since last indexed.
#[tauri::command]
async fn find_line(state: State<'_, AppState>, path: String, query: String) -> Result<usize, String> {
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    synthesize::best_matching_line(&state.embedder, &query, &content).map_err(|e| e.to_string())
}

/// Slices out `start_line..=end_line` (1-indexed, inclusive) of `path`, for
/// the "send to agent" button on a plain search result. Citations already
/// carry their chunk's text from the index, but `SearchResult` doesn't (it's
/// just path/score/line-range), so this re-reads the live file from disk,
/// same as `find_line` does, rather than adding chunk content to every
/// search response whether it's needed or not.
#[tauri::command]
fn read_chunk_preview(path: String, start_line: usize, end_line: usize) -> Result<String, String> {
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = content.lines().collect();
    let start = start_line.saturating_sub(1).min(lines.len());
    let end = end_line.min(lines.len()).max(start);
    Ok(lines[start..end].join("\n"))
}

// A hard cap, not a soft warning: past this many files, something has
// almost certainly been pointed at a home directory, a whole disk, or
// another huge unrelated tree by mistake (this happened during dev — a
// home-directory scan pegged every core for minutes before it was noticed).
const MAX_INDEXABLE_FILES: usize = 2000;
// Counting itself can hang — walking a real home directory during testing
// took 2+ minutes without finishing, almost certainly iCloud Drive
// materializing placeholder files on stat(). So the count runs on a
// dedicated thread with a hard wall-clock deadline; timing out is treated
// as "too big to safely watch", same as exceeding the file cap.
const COUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Rejects paths that are obviously too broad before touching the
/// filesystem at all — zero-cost, and catches the exact mistake that
/// prompted this guard (accidentally watching the whole home directory).
fn is_too_broad(path: &Path) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let normalized = path.to_string_lossy().trim_end_matches('/').to_string();
    let home = home.trim_end_matches('/');

    normalized.is_empty()
        || normalized == "/"
        || normalized == home
        || matches!(
            normalized.as_str(),
            "/Users" | "/System" | "/Library" | "/Applications" | "/Volumes"
        )
}

/// Starts a background thread that indexes `folder` and keeps watching it
/// for changes. Each distinct folder gets its own thread; calling this again
/// with a folder that's already being watched is a no-op. Refuses folders
/// that are obviously too broad, or that have an unreasonable number of
/// indexable files, rather than silently embedding all of them. Persists the
/// updated folder list to disk so it survives app restarts. Plain function
/// (not a command) so both the `start_watch` command and startup restoration
/// can share it without going through Tauri's `State` extractor.
fn start_watching_folder(state: &AppState, folder: String) -> Result<(), String> {
    let path = PathBuf::from(&folder);

    if is_too_broad(&path) {
        return Err(format!(
            "\"{}\" is too broad to index (e.g. your home directory or a whole volume) — pick a specific project folder instead",
            path.display()
        ));
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let count_path = path.clone();
    std::thread::spawn(move || {
        let _ = tx.send(watcher::count_indexable_files(&count_path, MAX_INDEXABLE_FILES));
    });
    match rx.recv_timeout(COUNT_TIMEOUT) {
        Ok(count) if count > MAX_INDEXABLE_FILES => {
            return Err(format!(
                "\"{}\" has more than {MAX_INDEXABLE_FILES} indexable files — pick a more specific folder",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(_) => {
            return Err(format!(
                "\"{}\" took too long to scan — pick a more specific folder",
                path.display()
            ));
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    // Held across the check, the thread spawn, and the insert, same as the
    // original check-then-insert did, so two concurrent calls for the same
    // folder can't both pass the guard before either registers — spawning
    // is just an OS call that returns immediately with a handle, it doesn't
    // block on the thread's actual work, so holding the lock through it is
    // cheap.
    let mut watching = state.watching.lock().map_err(|e| e.to_string())?;
    if watching.contains_key(&path) {
        return Ok(());
    }

    let embedder = state.embedder.clone();
    let store = state.store.clone();
    let thread_stop = stop.clone();
    let thread_path = path.clone();
    let thread = std::thread::spawn(move || {
        if let Err(e) = std::fs::create_dir_all(&thread_path) {
            eprintln!("failed to create watch folder: {e}");
            return;
        }
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("failed to start watch runtime: {e}");
                return;
            }
        };
        if let Err(e) = rt.block_on(watcher::watch(&thread_path, &embedder, &store, thread_stop)) {
            eprintln!("watch loop exited with error: {e}");
        }
    });

    watching.insert(path, WatchHandle { stop, thread });
    drop(watching);
    save_watched_folders(state);

    Ok(())
}

#[tauri::command]
fn start_watch(state: State<'_, AppState>, folder: String) -> Result<(), String> {
    start_watching_folder(&state, folder)
}

/// Stops watching `folder`: signals its background thread to exit, waits
/// for it to actually confirm that (up to ~300ms, see `watcher::watch`'s
/// loop), drops it from the persisted folder list, and only then purges
/// every already-indexed row under it. The wait matters: without it, an
/// already-queued filesystem event the thread was mid-processing can call
/// `upsert` *after* the purge runs, silently resurrecting a row for a
/// folder that was just removed.
#[tauri::command]
async fn stop_watch(state: State<'_, AppState>, folder: String) -> Result<(), String> {
    let path = PathBuf::from(&folder);
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

    let watch_handle = {
        let mut watching = state.watching.lock().map_err(|e| e.to_string())?;
        watching.remove(&path)
    };

    if let Some(WatchHandle { stop, thread }) = watch_handle {
        stop.store(true, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            // Nothing meaningful to do with a join error (thread panicked);
            // either way the thread is no longer running, which is all
            // we're waiting to confirm here.
            let _ = thread.join();
        })
        .await
        .map_err(|e| e.to_string())?;
    }

    save_watched_folders(&state);

    state
        .store
        .delete_under(&canonical.to_string_lossy())
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
fn list_watched(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let watching = state.watching.lock().map_err(|e| e.to_string())?;
    Ok(watching.keys().map(|p| p.to_string_lossy().to_string()).collect())
}

#[tauri::command]
fn home_dir() -> Result<String, String> {
    std::env::var("HOME").map_err(|e| e.to_string())
}

/// Given a partial path being typed (e.g. "/Users/rahul/Doc"), returns
/// matching subdirectories of the nearest existing parent, for inline
/// tab-completion. Directories only, hidden entries excluded unless the
/// user's already typing a name starting with '.'.
#[tauri::command]
fn list_dir_suggestions(partial: String) -> Result<Vec<String>, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let expanded = if partial == "~" {
        home.clone()
    } else if let Some(rest) = partial.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        partial.clone()
    };

    let path = PathBuf::from(&expanded);
    let (dir, prefix) = if expanded.ends_with('/') {
        (path.clone(), String::new())
    } else {
        let parent = path.parent().unwrap_or(std::path::Path::new("/"));
        let prefix = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        (parent.to_path_buf(), prefix)
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };

    let prefix_lower = prefix.to_lowercase();
    let mut matches: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let hidden = name.starts_with('.');
            let user_wants_hidden = prefix.starts_with('.');
            let matches_prefix = name.to_lowercase().starts_with(&prefix_lower);

            (matches_prefix && (!hidden || user_wants_hidden))
                .then(|| format!("{}/", e.path().to_string_lossy()))
        })
        .collect();

    matches.sort();
    matches.truncate(15);
    Ok(matches)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Shared with the CLI (`~/.reference/`) so `reference-cli search` reads
    // the same index this app's watchers populate, instead of each keeping
    // its own disconnected database. Also sidesteps the old problem this
    // constant used to work around: Tauri's dev watcher rebuilds+restarts
    // the app on any change under src-tauri/, and LanceDB writes temp files
    // on every upsert — a path under $HOME is outside that tree entirely.
    std::fs::create_dir_all(paths::default_app_data_dir())
        .expect("failed to create app data directory");

    println!("loading embedding model (all-MiniLM-L6-v2)...");
    let embedder =
        tauri::async_runtime::block_on(Embedder::load()).expect("failed to load embedding model");
    let store = tauri::async_runtime::block_on(Store::open(&paths::default_db_uri()))
        .expect("failed to open lancedb store");

    let state = AppState {
        embedder: Arc::new(embedder),
        store: Arc::new(store),
        watching: Mutex::new(HashMap::new()),
    };

    for folder in load_watched_folders() {
        if let Err(e) = start_watching_folder(&state, folder.clone()) {
            eprintln!("failed to restore watch for {folder}: {e}");
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            search,
            find_line,
            read_chunk_preview,
            start_watch,
            stop_watch,
            list_watched,
            home_dir,
            list_dir_suggestions
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
