//! Compile-time regression guard for §3.5 actor propagation.
//!
//! Walks `src/routes/**/*.rs` and fails the build if any file contains a
//! bare `Actor::user()` call.  All dispatch sites must go through
//! `actor_from(&auth, ...)` so that Bot tokens are attributed as
//! `Actor::Agent` rather than silently collapsed to `Actor::User`.

use std::path::Path;

fn main() {
    // Re-run whenever any routes source file changes.
    println!("cargo:rerun-if-changed=src/routes");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let routes_dir = Path::new(&manifest).join("src").join("routes");

    if let Err(e) = check_dir(&routes_dir) {
        // cargo:error= lines are highlighted in the build output.
        println!("cargo:error={e}");
        std::process::exit(1);
    }
}

fn check_dir(dir: &Path) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            check_dir(&path)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            check_file(&path)?;
        }
    }
    Ok(())
}

fn check_file(path: &Path) -> Result<(), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    for (lineno, line) in content.lines().enumerate() {
        if line.contains("Actor::user()") {
            return Err(format!(
                "§3.5 actor guard: Actor::user() found in {}:{} — \
                 use actor_from(&auth, None) so Bot tokens are attributed correctly.",
                path.display(),
                lineno + 1,
            ));
        }
    }
    Ok(())
}
