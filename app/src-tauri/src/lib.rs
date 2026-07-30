use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use reference_core::embedding::{Embedder, EmbeddingModel};
use reference_core::paths;
use reference_core::store::{RankingWeights, Store};
use reference_core::synthesize;
use reference_core::watcher;
use serde::{Deserialize, Serialize};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, PhysicalPosition, State, WindowEvent};

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
    // `Mutex<Arc<_>>`, not a plain `Arc<Embedder>`: `set_embedding_model`
    // needs to swap in a whole new `Embedder` at runtime (a different
    // model's weights/tokenizer, not something you can mutate in place).
    // Callers lock just long enough to clone the `Arc` out, then drop the
    // lock before doing any actual embedding work, so a model switch never
    // blocks a search beyond that clone.
    embedder: Mutex<Arc<Embedder>>,
    // Tracks which model is currently active, for `get_embedding_model` —
    // `Embedder` itself doesn't know/expose which `EmbeddingModel` variant
    // it was loaded from.
    current_model: Mutex<EmbeddingModel>,
    store: Arc<Store>,
    // Each entry gets its own background watch thread; re-adding an
    // already-watched folder is a no-op instead of spawning a duplicate.
    watching: Mutex<HashMap<PathBuf, WatchHandle>>,
    // Query-time only (see `RankingWeights`'s doc comment) — reading this
    // under a lock on every search is cheap, no need for anything fancier.
    ranking_weights: Mutex<RankingWeights>,
    // Populated by each folder's watch thread via `watcher::watch`'s
    // progress callback, so the frontend can poll `get_indexing_progress`
    // and show a real "still scanning" state instead of no signal at all
    // between "folder added" and "watching for changes" (the old terminal-
    // only message). `Arc<Mutex<_>>`, not a plain field behind `AppState`'s
    // own lock, so the callback (which runs on the watch thread, not the
    // command-handling thread) can hold its own clone independent of every
    // other field on `AppState`.
    indexing_progress: Arc<Mutex<HashMap<PathBuf, watcher::IndexProgress>>>,
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

// One file backs both settings — `#[serde(default)]` on each field means
// a file written before one of them existed (e.g. ranking_weights.json's
// predecessor before embedding_model was added) still parses fine, just
// with that field's default rather than failing the whole read the way an
// all-or-nothing struct would.
#[derive(Serialize, Deserialize, Default)]
struct AppSettings {
    #[serde(default)]
    ranking_weights: RankingWeights,
    #[serde(default)]
    embedding_model: EmbeddingModel,
}

fn load_app_settings() -> AppSettings {
    std::fs::read_to_string(paths::default_settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_app_settings(settings: &AppSettings) -> Result<(), String> {
    let json = serde_json::to_string(settings).map_err(|e| e.to_string())?;
    std::fs::write(paths::default_settings_path(), json).map_err(|e| e.to_string())
}

// Read-modify-write against the shared file, so setting one field never
// clobbers whatever the other one currently holds.
fn save_ranking_weights(weights: &RankingWeights) -> Result<(), String> {
    let mut settings = load_app_settings();
    settings.ranking_weights = *weights;
    save_app_settings(&settings)
}

fn save_embedding_model(model: EmbeddingModel) -> Result<(), String> {
    let mut settings = load_app_settings();
    settings.embedding_model = model;
    save_app_settings(&settings)
}

#[derive(Serialize)]
struct SearchResult {
    path: String,
    score: f32,
    start_line: usize,
    end_line: usize,
    chunk_kind: String,
    // True when this row came from the exact-symbol lookup (see
    // `looks_like_identifier`/`Store::find_by_name`), not the fuzzy/semantic
    // ranking — the UI badges these differently, since "this is literally
    // the thing you named" is a stronger claim than "this looks relevant".
    exact_match: bool,
    // True when this chunk exceeded the embedding model's token limit and
    // got silently cut down before embedding (see gap #4 in
    // docs/feature-gaps.md) — search may be missing content near the end of
    // this specific chunk, which is worth surfacing rather than leaving
    // invisible.
    truncated: bool,
}

#[derive(Serialize)]
struct Citation {
    path: String,
    snippet: String,
    start_line: usize,
    end_line: usize,
    chunk_kind: String,
    truncated: bool,
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

// A query is treated as "looking for this exact symbol" only when it's a
// single bare identifier — a shape a natural-language question or a
// multi-word filename-ish query never takes. Deliberately conservative:
// this gates a literal `name = '<query>'` lookup (see
// `Store::find_by_name`), so a false positive here wouldn't just rank
// something oddly, it would silently skip exact matching for a query that
// actually named a symbol (e.g. one with a leading digit, which is not a
// valid identifier in any of the supported languages anyway).
fn looks_like_identifier(query: &str) -> bool {
    let mut chars = query.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[tauri::command]
async fn search(
    state: State<'_, AppState>,
    query: String,
    top_k: usize,
    // Optional, same param the MCP `search` tool already has (see
    // mcp/src/main.rs). The UI only surfaces the scope picker once more than
    // one folder is watched — with zero or one, "scope to a folder" and
    // "search everything" mean the same thing, so there's nothing to pick.
    folder: Option<String>,
) -> Result<SearchResponse, String> {
    // Exact-symbol lookup (gap: no IDE-style "find this exact function by
    // name" path — fuzzy/semantic/content-overlap ranking can't guarantee
    // literal identity). Deliberately app-only: the MCP `search` tool skips
    // this on purpose, since an agent already has grep for exact lookups
    // (see CLAUDE.md/feature-gaps.md's gap #3) — this exists for the human
    // in the desktop search palette who expects go-to-definition behavior.
    let exact_hits = if looks_like_identifier(&query) {
        state
            .store
            .find_by_name(&query, folder.as_deref())
            .await
            .map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };

    let weights = *state.ranking_weights.lock().map_err(|e| e.to_string())?;
    let embedder = state.embedder.lock().map_err(|e| e.to_string())?.clone();
    let embedding = embedder.embed(&query).map_err(|e| e.to_string())?;
    let hits = state
        .store
        .hybrid_search(
            &query,
            &embedding,
            top_k.max(SYNTHESIS_CANDIDATE_POOL),
            folder.as_deref(),
            &weights,
        )
        .await
        .map_err(|e| e.to_string())?;

    let citations = if synthesize::is_question(&query) && !hits.is_empty() {
        synthesize::synthesize(&embedder, &query, &hits)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|c| Citation {
                path: c.path,
                snippet: c.snippet,
                start_line: c.start_line,
                end_line: c.end_line,
                chunk_kind: c.chunk_kind,
                truncated: c.truncated,
            })
            .collect()
    } else {
        Vec::new()
    };

    // Exact matches lead, since they're a stronger claim than anything
    // ranked — then whatever hybrid_search found, skipping rows exact
    // lookup already surfaced (same path + line range) so nothing repeats.
    let exact_keys: std::collections::HashSet<(String, i32)> =
        exact_hits.iter().map(|h| (h.path.clone(), h.start_line)).collect();

    let results = exact_hits
        .into_iter()
        .map(|h| SearchResult {
            path: h.path,
            score: h.score,
            start_line: h.start_line as usize,
            end_line: h.end_line as usize,
            chunk_kind: h.chunk_kind,
            exact_match: true,
            truncated: h.truncated,
        })
        .chain(
            hits.into_iter()
                .filter(|h| !exact_keys.contains(&(h.path.clone(), h.start_line)))
                .map(|h| SearchResult {
                    path: h.path,
                    score: h.score,
                    start_line: h.start_line as usize,
                    end_line: h.end_line as usize,
                    chunk_kind: h.chunk_kind,
                    exact_match: false,
                    truncated: h.truncated,
                }),
        )
        .take(top_k)
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
    let embedder = state.embedder.lock().map_err(|e| e.to_string())?.clone();
    synthesize::best_matching_line(&embedder, &query, &content).map_err(|e| e.to_string())
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
    let total_files = match rx.recv_timeout(COUNT_TIMEOUT) {
        Ok(count) if count > MAX_INDEXABLE_FILES => {
            return Err(format!(
                "\"{}\" has more than {MAX_INDEXABLE_FILES} indexable files — pick a more specific folder",
                path.display()
            ));
        }
        Ok(count) => count,
        Err(_) => {
            return Err(format!(
                "\"{}\" took too long to scan — pick a more specific folder",
                path.display()
            ));
        }
    };

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

    let embedder = state.embedder.lock().map_err(|e| e.to_string())?.clone();
    let store = state.store.clone();
    let thread_stop = stop.clone();
    let thread_path = path.clone();
    let progress_map = state.indexing_progress.clone();
    let progress_path = path.clone();
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
        let on_progress = {
            let progress_map = progress_map.clone();
            let progress_path = progress_path.clone();
            move |p: watcher::IndexProgress| {
                if let Ok(mut map) = progress_map.lock() {
                    // Once the scan is actually done, drop the entry rather
                    // than leaving it parked at indexed==total forever —
                    // an entry in this map means "still scanning", and a
                    // finished folder has nothing left to report until it's
                    // unwatched/re-watched.
                    if p.done {
                        map.remove(&progress_path);
                    } else {
                        map.insert(progress_path.clone(), p);
                    }
                }
            }
        };
        if let Err(e) = rt.block_on(watcher::watch(
            &thread_path,
            &embedder,
            &store,
            thread_stop,
            total_files,
            on_progress,
        )) {
            eprintln!("watch loop exited with error: {e}");
        }
        // Covers both a clean stop (see `stop_watching_folder`, which
        // removes this entry itself right after signalling) and an
        // unexpected early exit (a `watch()` error) — either way, no
        // thread is left updating this folder's entry, so it shouldn't
        // linger in the map suggesting otherwise.
        if let Ok(mut map) = progress_map.lock() {
            map.remove(&progress_path);
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
/// folder that was just removed. Plain function (not a command), same
/// reasoning as `start_watching_folder`: `set_embedding_model` needs this
/// same stop-then-restart sequence per folder to force a full reindex
/// under the new model, without going through Tauri's `State` extractor
/// twice in one command.
async fn stop_watching_folder(state: &AppState, folder: String) -> Result<(), String> {
    let path = PathBuf::from(&folder);
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

    let watch_handle = {
        let mut watching = state.watching.lock().map_err(|e| e.to_string())?;
        watching.remove(&path)
    };
    if let Ok(mut progress) = state.indexing_progress.lock() {
        progress.remove(&path);
    }

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

    save_watched_folders(state);

    state
        .store
        .delete_under(&canonical.to_string_lossy())
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
async fn stop_watch(state: State<'_, AppState>, folder: String) -> Result<(), String> {
    stop_watching_folder(&state, folder).await
}

#[tauri::command]
fn list_watched(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let watching = state.watching.lock().map_err(|e| e.to_string())?;
    Ok(watching.keys().map(|p| p.to_string_lossy().to_string()).collect())
}

/// Lets the frontend poll whether a watched folder's initial scan has
/// finished — `watcher::watch` used to only ever print this to the
/// terminal ("watching X for changes..."), so a search run immediately
/// after adding a folder could silently miss files still queued for
/// embedding. Only folders currently mid-scan (or that never got removed
/// after a `watch()` error, cleaned up by the thread itself) show up here;
/// a fully-watched folder with no entry means its scan already completed.
#[tauri::command]
fn get_indexing_progress(
    state: State<'_, AppState>,
) -> Result<HashMap<String, watcher::IndexProgress>, String> {
    let progress = state.indexing_progress.lock().map_err(|e| e.to_string())?;
    Ok(progress
        .iter()
        .map(|(path, p)| (path.to_string_lossy().to_string(), *p))
        .collect())
}

#[tauri::command]
fn get_ranking_weights(state: State<'_, AppState>) -> Result<RankingWeights, String> {
    state.ranking_weights.lock().map(|w| *w).map_err(|e| e.to_string())
}

/// Persists `weights` to disk and applies them to every search from this
/// point on. Query-time only (see `RankingWeights`'s doc comment in
/// store.rs) — no index rebuild, no restart needed, unlike gap #3/#4's
/// schema columns.
#[tauri::command]
fn set_ranking_weights(state: State<'_, AppState>, weights: RankingWeights) -> Result<(), String> {
    save_ranking_weights(&weights)?;
    *state.ranking_weights.lock().map_err(|e| e.to_string())? = weights;
    Ok(())
}

#[derive(Serialize)]
struct EmbeddingModelInfo {
    id: EmbeddingModel,
    display_name: &'static str,
}

/// Every model the app lets a user pick from — all 384-dim, all
/// BERT-architecture (see `EmbeddingModel`'s doc comment in
/// core/src/embedding.rs for why those two constraints exist).
#[tauri::command]
fn list_embedding_models() -> Vec<EmbeddingModelInfo> {
    [
        EmbeddingModel::MiniLmL6,
        EmbeddingModel::MiniLmL12,
        EmbeddingModel::BgeSmall,
        EmbeddingModel::GteSmall,
    ]
    .into_iter()
    .map(|id| EmbeddingModelInfo { id, display_name: id.display_name() })
    .collect()
}

#[tauri::command]
fn get_embedding_model(state: State<'_, AppState>) -> Result<EmbeddingModel, String> {
    state.current_model.lock().map(|m| *m).map_err(|e| e.to_string())
}

/// Switches the active embedding model, persists the choice, and forces a
/// full reindex of every currently-watched folder. Not optional the way
/// `set_ranking_weights` is a no-reindex change: two different models'
/// vectors aren't comparable even at the same 384 dimension (see gap #5,
/// docs/feature-gaps.md), so leaving the old index in place would mean
/// every search silently scores query embeddings from the new model
/// against stored embeddings from the old one — wrong results, not just
/// stale ones, and nothing about that would look like an error.
///
/// Reuses the same stop-then-start sequence a folder goes through when
/// manually un/re-watched (`stop_watching_folder` purges that folder's
/// rows, `start_watching_folder` does a full initial scan+embed) rather
/// than a bespoke whole-table wipe — one folder failing to reindex doesn't
/// corrupt the others, and it's a code path that's already exercised by
/// normal watch/unwatch.
#[tauri::command]
async fn set_embedding_model(state: State<'_, AppState>, model: EmbeddingModel) -> Result<(), String> {
    save_embedding_model(model)?;

    let new_embedder = Embedder::load(model).await.map_err(|e| e.to_string())?;
    *state.embedder.lock().map_err(|e| e.to_string())? = Arc::new(new_embedder);
    *state.current_model.lock().map_err(|e| e.to_string())? = model;

    let folders: Vec<String> = {
        let watching = state.watching.lock().map_err(|e| e.to_string())?;
        watching.keys().map(|p| p.to_string_lossy().to_string()).collect()
    };

    for folder in folders {
        stop_watching_folder(&state, folder.clone()).await?;
        start_watching_folder(&state, folder)?;
    }

    Ok(())
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

    let settings = load_app_settings();

    println!("loading embedding model ({})...", settings.embedding_model.display_name());
    let embedder = tauri::async_runtime::block_on(Embedder::load(settings.embedding_model))
        .expect("failed to load embedding model");
    let store = tauri::async_runtime::block_on(Store::open(&paths::default_db_uri()))
        .expect("failed to open lancedb store");

    let state = AppState {
        embedder: Mutex::new(Arc::new(embedder)),
        current_model: Mutex::new(settings.embedding_model),
        store: Arc::new(store),
        watching: Mutex::new(HashMap::new()),
        ranking_weights: Mutex::new(settings.ranking_weights),
        indexing_progress: Arc::new(Mutex::new(HashMap::new())),
    };

    for folder in load_watched_folders() {
        if let Err(e) = start_watching_folder(&state, folder.clone()) {
            eprintln!("failed to restore watch for {folder}: {e}");
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(state)
        .setup(|app| {
            // The window starts hidden (`"visible": false` in
            // tauri.conf.json) specifically so this can position it before
            // first paint — showing it centered (the old default) and then
            // jumping it up here would flash visibly. Positioned near the
            // top of the screen's work area (excludes the menu bar/dock),
            // not dead center, matching where a launcher like Spotlight/
            // Raycast/Alfred conventionally appears rather than the middle
            // of the screen.
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(Some(monitor)) = window.primary_monitor() {
                    let work_area = monitor.work_area();
                    let scale = monitor.scale_factor();
                    let window_width_px = (680.0 * scale).round() as i32;
                    let x = work_area.position.x
                        + (work_area.size.width as i32 - window_width_px) / 2;
                    let y = work_area.position.y + (work_area.size.height as f64 * 0.12) as i32;
                    let _ = window.set_position(PhysicalPosition::new(x, y));
                }
                let _ = window.show();
            }

            // Menu bar icon so the window can be brought back after being
            // closed (see the `on_window_event` below, which hides "main"
            // instead of destroying it on close specifically so there's
            // still a window for this to show/focus). `icon_as_template`
            // is macOS-only: it tells the OS to render just the icon's
            // alpha-channel silhouette in solid black/white, adapting to
            // light/dark menu bar automatically, instead of showing the
            // icon's actual (light gray) pixel colors verbatim.
            TrayIconBuilder::new()
                .icon(tauri::include_image!("icons/tray-icon.png"))
                .icon_as_template(true)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing "main" hides it rather than destroying it, so the
            // tray icon's click handler always has a window to show/focus
            // instead of needing to recreate one from scratch.
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            search,
            find_line,
            read_chunk_preview,
            start_watch,
            stop_watch,
            list_watched,
            get_indexing_progress,
            home_dir,
            list_dir_suggestions,
            get_ranking_weights,
            set_ranking_weights,
            list_embedding_models,
            get_embedding_model,
            set_embedding_model
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
