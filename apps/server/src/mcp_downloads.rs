//! Resolve paths to the bundled unified `daruma` binary for HTTP download.

use std::path::{Path, PathBuf};

/// Resolved download paths discovered at startup.
#[derive(Clone, Debug, Default)]
pub struct McpDownloads {
    pub linux: Option<PathBuf>,
    pub windows: Option<PathBuf>,
}

impl McpDownloads {
    /// Resolve binaries from explicit env vars and conventional directories.
    pub fn discover() -> Self {
        let mut out = Self::default();

        if let Ok(p) = std::env::var("DARUMA_MCP_BIN_LINUX") {
            let path = PathBuf::from(p);
            if path.is_file() {
                out.linux = Some(path);
            }
        }
        if let Ok(p) = std::env::var("DARUMA_MCP_BIN_WINDOWS") {
            let path = PathBuf::from(p);
            if path.is_file() {
                out.windows = Some(path);
            }
        }

        if out.linux.is_none() || out.windows.is_none() {
            let dir = std::env::var("DARUMA_MCP_BIN_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/app/bin"));

            if out.linux.is_none() {
                for name in ["daruma-linux", "daruma"] {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        out.linux = Some(candidate);
                        break;
                    }
                }
            }
            if out.windows.is_none() {
                for name in ["daruma-windows.exe", "daruma.exe"] {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        out.windows = Some(candidate);
                        break;
                    }
                }
            }
        }

        // Local dev: cargo build -p daruma-cli
        if out.linux.is_none() {
            out.linux = dev_release_binary("daruma");
        }
        if out.windows.is_none() {
            out.windows = dev_release_binary("daruma.exe");
        }

        out
    }

    pub fn path_for(&self, platform: &str) -> Option<&Path> {
        match platform {
            "linux" => self.linux.as_deref(),
            "windows" => self.windows.as_deref(),
            _ => None,
        }
    }
}

fn dev_release_binary(name: &str) -> Option<PathBuf> {
    for base in [
        "target/release",
        "../target/release",
        "../../target/release",
    ] {
        let candidate = PathBuf::from(base).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_for_platforms() {
        let dl = McpDownloads {
            linux: Some(PathBuf::from("/tmp/daruma-mcp")),
            windows: Some(PathBuf::from("/tmp/daruma-mcp.exe")),
        };
        assert!(dl.path_for("linux").is_some());
        assert!(dl.path_for("windows").is_some());
        assert!(dl.path_for("macos").is_none());
    }
}
