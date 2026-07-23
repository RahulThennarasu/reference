use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use reference_core::embedding::Embedder;
use reference_core::store::Store;
use reference_core::synthesize;
use reference_core::watcher;
use serde::Serialize;
use tauri::{Manager, State};
#[cfg(target_os = "macos")]
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};

// Deliberately outside src-tauri/: Tauri's dev watcher rebuilds+restarts the
// app on any change under src-tauri/, and LanceDB writes temp files on every
// upsert, so pointing the index inside src-tauri/ causes a restart loop.
// This should move to a real app-data directory (via tauri's path resolver)
// before shipping.
const DB_URI: &str = "../data/reference-index";
const WATCHED_FOLDERS_FILE: &str = "../data/watched_folders.json";

struct AppState {
    embedder: Arc<Embedder>,
    store: Arc<Store>,
    // Each entry gets its own background watch thread plus the flag used to
    // stop it; re-adding an already-watched folder is a no-op instead of
    // spawning a duplicate.
    watching: Mutex<HashMap<PathBuf, Arc<AtomicBool>>>,
}

fn load_watched_folders() -> Vec<String> {
    std::fs::read_to_string(WATCHED_FOLDERS_FILE)
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
            if let Err(e) = std::fs::write(WATCHED_FOLDERS_FILE, json) {
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
}

#[derive(Serialize)]
struct Citation {
    path: String,
    snippet: String,
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
            .map(|c| Citation { path: c.path, snippet: c.snippet })
            .collect()
    } else {
        Vec::new()
    };

    let results = hits
        .into_iter()
        .take(top_k)
        .map(|h| SearchResult { path: h.path, score: h.score })
        .collect();

    Ok(SearchResponse { results, citations })
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
    {
        let mut watching = state.watching.lock().map_err(|e| e.to_string())?;
        if watching.contains_key(&path) {
            return Ok(());
        }
        watching.insert(path.clone(), stop.clone());
    }
    save_watched_folders(state);

    let embedder = state.embedder.clone();
    let store = state.store.clone();
    std::thread::spawn(move || {
        if let Err(e) = std::fs::create_dir_all(&path) {
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
        if let Err(e) = rt.block_on(watcher::watch(&path, &embedder, &store, stop)) {
            eprintln!("watch loop exited with error: {e}");
        }
    });

    Ok(())
}

#[tauri::command]
fn start_watch(state: State<'_, AppState>, folder: String) -> Result<(), String> {
    start_watching_folder(&state, folder)
}

/// Stops watching `folder`: signals its background thread to exit (it
/// notices within ~300ms) and drops it from the persisted folder list.
#[tauri::command]
fn stop_watch(state: State<'_, AppState>, folder: String) -> Result<(), String> {
    let path = PathBuf::from(&folder);
    {
        let mut watching = state.watching.lock().map_err(|e| e.to_string())?;
        if let Some(stop) = watching.remove(&path) {
            stop.store(true, Ordering::Relaxed);
        }
    }
    save_watched_folders(&state);
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
    println!("loading embedding model (all-MiniLM-L6-v2)...");
    let embedder =
        tauri::async_runtime::block_on(Embedder::load()).expect("failed to load embedding model");
    let store =
        tauri::async_runtime::block_on(Store::open(DB_URI)).expect("failed to open lancedb store");

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
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            search,
            start_watch,
            stop_watch,
            list_watched,
            home_dir,
            list_dir_suggestions
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                let window = app.get_webview_window("main").expect("main window missing");
                apply_vibrancy(&window, NSVisualEffectMaterial::Popover, None, Some(16.0))
                    .expect("failed to apply macOS vibrancy");
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
