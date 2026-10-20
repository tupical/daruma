//! README «## MCP» перечисляет домены, доступные как MCP-инструменты. Каждый
//! перечисленный домен обязан иметь инструменты в каталоге: иначе README
//! обещает то, что есть только в REST (как было с webhooks).

use std::{fs, path::PathBuf};

use daruma_mcp::tools::tool_definitions;

/// Домен из README → подстрока имени инструмента, которая его подтверждает.
const DOMAIN_TOOL_MARKERS: &[(&str, &str)] = &[
    ("tasks", "daruma_list"),
    ("plans", "daruma_plan_"),
    ("documents", "daruma_doc_"),
    ("sessions", "daruma_session_"),
    ("runs", "daruma_run_"),
    ("webhooks", "webhook"),
];

fn readme_mcp_surface() -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md");
    let readme = fs::read_to_string(&path).expect("read README.md");
    let section = readme
        .split("\n## MCP\n")
        .nth(1)
        .expect("README has a `## MCP` section");
    let flat = section.split_whitespace().collect::<Vec<_>>().join(" ");
    let list = flat
        .split("The same tool surface — ")
        .nth(1)
        .and_then(|rest| rest.split(" — ").next())
        .expect("README MCP section lists the tool surface between em dashes");
    list.split(',')
        .map(|item| item.trim().to_string())
        .collect()
}

#[test]
fn readme_mcp_surface_lists_only_domains_with_mcp_tools() {
    let names: Vec<&str> = tool_definitions().iter().map(|t| t.name).collect();
    let surface = readme_mcp_surface();
    assert!(!surface.is_empty(), "README MCP tool surface is empty");

    for domain in &surface {
        let marker = DOMAIN_TOOL_MARKERS
            .iter()
            .find(|(d, _)| d == domain)
            .map(|(_, m)| *m)
            .unwrap_or_else(|| panic!("README MCP surface lists unknown domain `{domain}`"));
        assert!(
            names.iter().any(|n| n.contains(marker)),
            "README MCP surface lists `{domain}`, but no MCP tool matches `{marker}`"
        );
    }
}

#[test]
fn webhooks_are_not_an_mcp_tool_domain() {
    assert!(
        !tool_definitions()
            .iter()
            .any(|t| t.name.contains("webhook")),
        "webhook MCP tools exist now — list them in README `## MCP`"
    );
    assert!(
        !readme_mcp_surface().iter().any(|d| d == "webhooks"),
        "README MCP surface lists webhooks, which are REST-only (/v1/webhooks)"
    );
}
