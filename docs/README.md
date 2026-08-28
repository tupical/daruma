# Documentation

Daruma is the execution layer of Meisei — crafted for speed and collaboration
with humans and AI.

## Core docs

| Document | Purpose |
| --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Canonical architecture contract and crate responsibilities. |
| [MODULES.md](MODULES.md) | Module registry and ownership boundaries. |
| [MODULE_CONTRACT.md](MODULE_CONTRACT.md) | Compatibility rules for core and module changes. |
| [architecture-policy.md](architecture-policy.md) | Fixed policy decisions for backfill, cascade, and sequence IDs. |
| [LIFECYCLE_RULES_SPEC.md](LIFECYCLE_RULES_SPEC.md) | Lifecycle rules engine: statuses, gates, evidence. |
| [DEPLOYMENT.md](DEPLOYMENT.md) | Self-host deployment: ports, TLS, systemd. |
| [RELEASES.md](RELEASES.md) | Release checklist and tagging flow. |
| [ROADMAP_FEATURE_GAP.md](ROADMAP_FEATURE_GAP.md) | Feature-gap notes vs. the product roadmap. |
| [VERSION_HISTORY.md](VERSION_HISTORY.md) | Versioned task and document record history. |

## Guides

| Document | Purpose |
| --- | --- |
| [guides/mcp-client.md](guides/mcp-client.md) | MCP client setup and credentials. |
| [guides/ai-agent.md](guides/ai-agent.md) | AI layer rules and tools. |
| [guides/agent-session-metadata.md](guides/agent-session-metadata.md) | Agent session metadata contract. |
| [guides/comment-conventions.md](guides/comment-conventions.md) | Comment prefix conventions for agents. |
| [guides/documents-auto-append.md](guides/documents-auto-append.md) | Interview / Human Log auto-append. |
| [guides/local-dev-data.md](guides/local-dev-data.md) | Local dev data layout and backup. |

## MCP

| Document | Purpose |
| --- | --- |
| [mcp/PROFILES.md](mcp/PROFILES.md) | Tool-surface profiles (`default` / `full`). |
| [mcp/FEATURE-TIERS.md](mcp/FEATURE-TIERS.md) | Machine-generated tool tier matrix. |
| [mcp/TOKEN-ECONOMY.md](mcp/TOKEN-ECONOMY.md) | Token-economy rules for agent calls. |
| [mcp/EXECUTOR-LOOP.md](mcp/EXECUTOR-LOOP.md) | Canonical agent executor loop. |

## Architecture decision records

| Document | Purpose |
| --- | --- |
| [adr/terminal-execution-layer.md](adr/terminal-execution-layer.md) | Daruma as the terminal execution layer. |
| [adr/parallel-agent-isolation.md](adr/parallel-agent-isolation.md) | Parallel agent isolation model. |
| [adr/work-units-and-artifacts.md](adr/work-units-and-artifacts.md) | Work units and artifact registry. |
| [adr/workspacegraph.md](adr/workspacegraph.md) | WorkspaceGraph structural graph. |

## Russian docs

Russian notes live in `*.ru.md` files:

- [README.ru.md](README.ru.md)
- [architecture-policy.ru.md](architecture-policy.ru.md)
- [ROADMAP_FEATURE_GAP.ru.md](ROADMAP_FEATURE_GAP.ru.md)
- [../CHANGELOG.ru.md](../CHANGELOG.ru.md)
