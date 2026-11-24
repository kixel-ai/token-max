use std::path::{Path, PathBuf};

/// `path` with the platform's separators, to compare with a path the code under test built, such
/// as `native_path("~/.claude/settings.json")` or `native_path(&w.home.join("repo/a.json"))`.
pub fn native_path(path: impl AsRef<Path>) -> String {
    // Rebuilt from components, since a path joined from "a/b" keeps the `/` on Windows.
    let path: PathBuf = path.as_ref().components().collect();
    path.to_string_lossy().into_owned()
}
