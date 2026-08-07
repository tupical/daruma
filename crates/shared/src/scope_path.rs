//! Canonical form for client-supplied repo paths (`scope_path`, `root_path`).
//!
//! MCP clients run on the user's machine; the server may run anywhere. A
//! Windows client sends `c:\Repos\app` (or `C:/Repos/app`), which `std`'s
//! `Path::is_absolute` rejects on a Linux server — the path then looks
//! "relative", and scope resolution fails or, worse, binds the same repo
//! twice under two spellings. Both sides normalize through here so a repo
//! has exactly one key regardless of the OS that named it.

/// True when `path` is absolute for *any* platform: POSIX (`/x`), Windows
/// drive-qualified (`C:\x`, `c:/x`), or UNC (`\\host\share`).
///
/// Deliberately not `Path::is_absolute` — that answers for the *running*
/// platform, and either side of this wire may be the other OS.
pub fn is_absolute_scope_path(path: &str) -> bool {
    let p = path.trim();
    p.starts_with('/')
        || p.starts_with("\\\\")
        || matches!(p.as_bytes(), [d, b':', b'/' | b'\\', ..] if d.is_ascii_alphabetic())
}

/// Canonical key for a repo path: `\` → `/`, collapsed duplicate separators,
/// upper-cased drive letter, no trailing slash. Case is otherwise preserved
/// (Linux is case-sensitive). A POSIX path normalizes to itself.
pub fn normalize_scope_path(path: &str) -> String {
    let unified = path.trim().replace('\\', "/");
    // A UNC path keeps its leading `//`; every other run of slashes collapses.
    let (prefix, rest) = match unified.strip_prefix("//") {
        Some(rest) => ("//", rest),
        None => ("", unified.as_str()),
    };
    let leading_slash = prefix.is_empty() && rest.starts_with('/');

    let mut out = String::with_capacity(unified.len());
    out.push_str(prefix);
    if leading_slash {
        out.push('/');
    }
    let mut segments = rest.split('/').filter(|s| !s.is_empty()).peekable();
    if let Some(first) = segments.peek() {
        // `c:` → `C:` so the same drive is one key.
        if is_drive(first) {
            out.push_str(&first.to_ascii_uppercase());
            segments.next();
            if segments.peek().is_some() {
                out.push('/');
            }
        }
    }
    out.push_str(&segments.collect::<Vec<_>>().join("/"));

    if out.is_empty() {
        return "/".to_string();
    }
    // `C:` alone is a drive-relative path in Windows; keep it rooted.
    if out.ends_with(':') && is_drive(&out) {
        out.push('/');
    }
    out
}

fn is_drive(seg: &str) -> bool {
    matches!(seg.as_bytes(), [d, b':'] if d.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_paths_are_unchanged() {
        assert_eq!(
            normalize_scope_path("/home/av/projects"),
            "/home/av/projects"
        );
        assert_eq!(
            normalize_scope_path("/home/av/projects/"),
            "/home/av/projects"
        );
        assert_eq!(normalize_scope_path("  /srv/repo  "), "/srv/repo");
        assert_eq!(normalize_scope_path("/srv//repo"), "/srv/repo");
        assert_eq!(normalize_scope_path("/"), "/");
        assert!(is_absolute_scope_path("/srv/repo"));
    }

    #[test]
    fn windows_spellings_collapse_to_one_key() {
        let expected = "C:/OSPanel/domains/investprojects.local";
        for raw in [
            r"c:\OSPanel\domains\investprojects.local",
            "C:/OSPanel/domains/investprojects.local",
            r"C:\OSPanel\domains\investprojects.local\",
            r"c:\OSPanel\\domains\investprojects.local",
        ] {
            assert_eq!(normalize_scope_path(raw), expected, "raw = {raw}");
            assert!(is_absolute_scope_path(raw), "raw = {raw}");
        }
        assert_eq!(normalize_scope_path(r"c:\"), "C:/");
        assert_eq!(normalize_scope_path("D:"), "D:/");
    }

    #[test]
    fn unc_paths_keep_their_double_slash() {
        assert_eq!(
            normalize_scope_path(r"\\build\share\repo"),
            "//build/share/repo"
        );
        assert!(is_absolute_scope_path(r"\\build\share"));
    }

    #[test]
    fn relative_paths_stay_relative() {
        for raw in ["repo", "./repo", "../repo", "repo/sub"] {
            assert!(!is_absolute_scope_path(raw), "raw = {raw}");
        }
        // `C:repo` is drive-relative, not absolute.
        assert!(!is_absolute_scope_path("C:repo"));
        assert_eq!(normalize_scope_path(r"sub\dir"), "sub/dir");
    }

    #[test]
    fn verdict_does_not_depend_on_the_running_platform() {
        // Both spellings must be absolute whichever OS evaluates them —
        // a POSIX client talking to a Windows host and vice versa.
        assert!(is_absolute_scope_path("/srv/repo"));
        assert!(is_absolute_scope_path(r"C:\repo"));
    }

    #[test]
    fn basename_survives_normalization() {
        // The server derives an auto-provisioned project title from this.
        let p = normalize_scope_path(r"c:\OSPanel\domains\investprojects.local");
        assert_eq!(
            std::path::Path::new(&p)
                .file_name()
                .and_then(|n| n.to_str()),
            Some("investprojects.local")
        );
    }
}
