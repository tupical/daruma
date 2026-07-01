use std::{env, fs, path::PathBuf};

use daruma_mcp::tools::{tool_definitions, Tier, ToolDefinition, ToolDomain};

const STALE_MSG: &str =
    "doc stale, run UPDATE_GOLDEN=1 cargo test -p daruma-mcp --test feature_tiers_doc";

#[test]
fn feature_tiers_doc_matches_catalogue() {
    let generated = build_markdown();
    let path = doc_path();

    if env::var_os("UPDATE_GOLDEN").as_deref() == Some(std::ffi::OsStr::new("1")) {
        fs::create_dir_all(path.parent().expect("doc path has parent")).unwrap();
        fs::write(&path, &generated).unwrap();
    }

    let existing = fs::read_to_string(&path).unwrap_or_else(|_| panic!("{STALE_MSG}"));
    assert_eq!(existing, generated, "{STALE_MSG}");
}

fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/mcp/FEATURE-TIERS.md")
}

fn build_markdown() -> String {
    let tools = tool_definitions();
    let domains = domains_in_catalogue_order(&tools);
    let mut out = String::new();

    out.push_str("# MCP feature tiers\n\n");
    out.push_str("АВТО-СГЕНЕРИРОВАНО из `crates/mcp/src/tools.rs` — не редактировать вручную; regenerate: `UPDATE_GOLDEN=1 cargo test -p daruma-mcp --test feature_tiers_doc`\n\n");
    out.push_str("Рамка: [feature-tiers.md](../../../meisei-research/docs/canon/feature-tiers.md).\n\n");
    out.push_str("Легенда: `Core` = основные; `Enhancing` = усиливающие; `Extending` = расширяющие.\n\n");

    out.push_str("## Сводная матрица\n\n");
    out.push_str("| domain | Core | Enhancing | Extending |\n");
    out.push_str("|---|---:|---:|---:|\n");
    for domain in &domains {
        let counts = counts_for(&tools, Some(*domain));
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            domain_label(*domain),
            counts[0],
            counts[1],
            counts[2]
        ));
    }
    let totals = counts_for(&tools, None);
    out.push_str(&format!(
        "| ИТОГО | {} | {} | {} |\n\n",
        totals[0], totals[1], totals[2]
    ));

    for domain in domains {
        out.push_str(&format!("## {}\n\n", domain_label(domain)));
        out.push_str("| tool | tier | profile | title | hints |\n");
        out.push_str("|---|---|---|---|---|\n");
        for tool in tools.iter().filter(|tool| tool.domain == domain) {
            out.push_str(&format!(
                "| `{}` | {} / {} | `{}` | {} | {} |\n",
                tool.name,
                tier_label(tool.tier),
                tool.tier.ru_label(),
                tool.profile.as_str(),
                tool.title,
                hints(tool)
            ));
        }
        out.push('\n');
    }

    out
}

fn domains_in_catalogue_order(tools: &[ToolDefinition]) -> Vec<ToolDomain> {
    let mut domains = Vec::new();
    for tool in tools {
        if !domains.contains(&tool.domain) {
            domains.push(tool.domain);
        }
    }
    domains
}

fn counts_for(tools: &[ToolDefinition], domain: Option<ToolDomain>) -> [usize; 3] {
    let mut counts = [0, 0, 0];
    for tool in tools {
        if domain.is_some_and(|domain| tool.domain != domain) {
            continue;
        }
        counts[tier_index(tool.tier)] += 1;
    }
    counts
}

fn tier_index(tier: Tier) -> usize {
    match tier {
        Tier::Core => 0,
        Tier::Enhancing => 1,
        Tier::Extending => 2,
    }
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Core => "Core",
        Tier::Enhancing => "Enhancing",
        Tier::Extending => "Extending",
    }
}

fn domain_label(domain: ToolDomain) -> &'static str {
    match domain {
        ToolDomain::Tasks => "Tasks",
        ToolDomain::Projects => "Projects",
        ToolDomain::Plans => "Plans",
        ToolDomain::Runs => "Runs",
        ToolDomain::Coordination => "Coordination",
        ToolDomain::Sessions => "Sessions",
        ToolDomain::Signals => "Signals",
        ToolDomain::Relations => "Relations",
        ToolDomain::WorkspaceGraph => "WorkspaceGraph",
        ToolDomain::Documents => "Documents",
        ToolDomain::History => "History",
        ToolDomain::Ai => "Ai",
        ToolDomain::Events => "Events",
        ToolDomain::Admin => "Admin",
    }
}

fn hints(tool: &ToolDefinition) -> &'static str {
    if tool.annotations.destructive_hint {
        "write,destructive"
    } else if tool.annotations.read_only_hint {
        "read"
    } else {
        "write"
    }
}
