//! Path-overlap logic for work leases — "like git, but lighter".
//!
//! Agents reserve repo-relative path globs while working a task; two
//! reservations conflict when their paths overlap. Granularity is at the
//! directory/file segment level (not byte-diff like git), which keeps the
//! check cheap while still preventing two agents from editing the same area.
//!
//! Matching rules (segment = component between `/`):
//! - Equal paths overlap.
//! - An ancestor overlaps its descendants (`src` vs `src/repo/a.rs`).
//! - A bare `*` segment matches exactly one segment (`src/*` vs `src/a.rs`).
//! - A `**` segment matches the remaining segments (`src/**` vs `src/a/b.rs`).
//! - Diverging siblings do not overlap (`src/a.rs` vs `src/b.rs`).
//! - The empty path (whole repo) overlaps everything.

/// Normalize a path glob: unify separators, drop `.` and empty segments,
/// strip leading/trailing slashes. Case is preserved (Linux is case-sensitive).
pub fn normalize_lease_path(p: &str) -> String {
    let unified = p.trim().replace('\\', "/");
    unified
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// True when two normalized path globs reserve overlapping areas of the tree.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let na = normalize_lease_path(a);
    let nb = normalize_lease_path(b);
    // An empty (whole-repo) reservation conflicts with anything.
    if na.is_empty() || nb.is_empty() {
        return true;
    }
    let sa: Vec<&str> = na.split('/').collect();
    let sb: Vec<&str> = nb.split('/').collect();
    overlap_segments(&sa, &sb)
}

fn segment_match(x: &str, y: &str) -> bool {
    x == y || x == "*" || y == "*"
}

fn overlap_segments(a: &[&str], b: &[&str]) -> bool {
    match (a.split_first(), b.split_first()) {
        // One path is a prefix of the other → ancestor/descendant → overlap.
        (None, _) | (_, None) => true,
        (Some((ha, ta)), Some((hb, tb))) => {
            // `**` swallows the remainder of either side.
            if *ha == "**" || *hb == "**" {
                return true;
            }
            if !segment_match(ha, hb) {
                return false;
            }
            overlap_segments(ta, tb)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_noise() {
        assert_eq!(normalize_lease_path("./src/repo/"), "src/repo");
        assert_eq!(normalize_lease_path("src//repo///a.rs"), "src/repo/a.rs");
        assert_eq!(normalize_lease_path("\\src\\repo"), "src/repo");
        assert_eq!(normalize_lease_path("  /src/  "), "src");
        assert_eq!(normalize_lease_path(""), "");
        assert_eq!(normalize_lease_path("."), "");
    }

    #[test]
    fn identical_paths_overlap() {
        assert!(paths_overlap("src/repo/a.rs", "src/repo/a.rs"));
        assert!(paths_overlap("./src/a.rs", "src/a.rs"));
    }

    #[test]
    fn ancestor_overlaps_descendant() {
        assert!(paths_overlap("src", "src/repo/a.rs"));
        assert!(paths_overlap("src/repo/a.rs", "src"));
        assert!(paths_overlap(
            "crates/storage",
            "crates/storage/src/claim_repo.rs"
        ));
    }

    #[test]
    fn diverging_siblings_do_not_overlap() {
        assert!(!paths_overlap("src/a.rs", "src/b.rs"));
        assert!(!paths_overlap("crates/storage", "crates/core"));
        assert!(!paths_overlap("apps/server/src", "apps/web/src"));
    }

    #[test]
    fn star_matches_single_segment() {
        assert!(paths_overlap("src/*", "src/a.rs"));
        assert!(paths_overlap("src/*/mod.rs", "src/repo/mod.rs"));
        assert!(!paths_overlap("src/*/mod.rs", "src/repo/lib.rs"));
    }

    #[test]
    fn double_star_matches_remainder() {
        assert!(paths_overlap("src/**", "src/a/b/c.rs"));
        assert!(paths_overlap("crates/**", "crates/core/src/path_lease.rs"));
        assert!(!paths_overlap("apps/**", "crates/core"));
    }

    #[test]
    fn whole_repo_overlaps_anything() {
        assert!(paths_overlap("", "anything/here.rs"));
        assert!(paths_overlap("/", "src"));
    }

    #[test]
    fn unrelated_top_level_dirs_are_independent() {
        // The point of leases: two agents in different subtrees never conflict.
        assert!(!paths_overlap("crates/storage/src", "crates/mcp/src"));
        assert!(!paths_overlap("docs", "src"));
    }
}
