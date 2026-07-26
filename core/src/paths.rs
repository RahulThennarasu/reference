use std::path::PathBuf;

/// Where the index lives by default — shared by the Tauri app and the CLI,
/// so `reference-cli search` reads the exact same index the app's watchers
/// populate, rather than each maintaining its own disconnected database.
/// A fixed dotfile directory under `$HOME` (rather than a path relative to
/// whatever directory a binary happens to be launched from) is what makes
/// the CLI usable as a general-purpose tool from any project directory.
pub fn default_app_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".reference")
}

pub fn default_db_uri() -> String {
    default_app_data_dir().join("index").to_string_lossy().to_string()
}

pub fn default_watched_folders_path() -> PathBuf {
    default_app_data_dir().join("watched_folders.json")
}

pub fn default_settings_path() -> PathBuf {
    default_app_data_dir().join("settings.json")
}
