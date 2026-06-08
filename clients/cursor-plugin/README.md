<p align="right">  <strong>English</strong> | <a href="./README.ru.md">RU</a>
</p>

<p align="center">
  <br/>
  ◯ ─────────── ◯
  <br/><br/>
  <strong>taskagent-cursor</strong>
  <br/>
  <sub>tupical/taskagent × Cursor</sub>
  <br/><br/>
  ◯ ─────────── ◯
  <br/>
</p>

<p align="center">
  <strong>One click wires the taskagent MCP server into Cursor.</strong>
  <br/>
  <sub>Drop-in Cursor wiring for the hosted taskagent MCP server.</sub>
</p>

---

## Add to Cursor

Click the button in any browser where Cursor is installed — Cursor will catch the
deeplink, show an approval dialog, and write the server into
`~/.cursor/mcp.json` for you.

<p align="center">
  <a href="https://cursor.com/install-mcp?name=taskagent&config=eyJ0eXBlIjoiaHR0cCIsInVybCI6Imh0dHA6Ly9sb2NhbGhvc3Q6ODA4MC92MS9tY3AifQ%3D%3D">
    <img src="https://img.shields.io/badge/Add%20to-Cursor-000000?style=for-the-badge&logo=cursor&logoColor=white" alt="Add to Cursor">
  </a>
</p>

Or the HTTPS mirror (works from any link unfurler):

```
https://cursor.com/install-mcp?name=taskagent&config=eyJ0eXBlIjoiaHR0cCIsInVybCI6Imh0dHA6Ly9sb2NhbGhvc3Q6ODA4MC92MS9tY3AifQ%3D%3D
```

The default Cursor path uses TaskAgent's HTTP MCP endpoint. For local
development, run the server first:

```bash
./target/release/taskagent-server   # data: ~/.agents/taskagent/data
```

---

## What it does

`taskagent-cursor` is a thin Cursor companion for
[`tupical/taskagent`](https://github.com/tupical/taskagent). It does three things:

1. **Registers the MCP server** in Cursor's `mcp.json` — globally
   (`~/.cursor/mcp.json`) or per-project (`./.cursor/mcp.json`).
2. **Generates "Add to Cursor" links** for one-click HTTP MCP setup.
3. **Drops three Cursor Rules** into `.cursor/rules/` that teach Cursor's
   agent how to drive `taskagent_*` tools — parse → decompose → plan →
   execute — instead of inventing its own task tracker, and keep it on the
   token-lean `list active` path:
   - `taskagent-policy.mdc` (`alwaysApply`) — taskagent is the default
     tracker; token-economy rules (list-first, no graph search for
     inventory).
   - `taskagent.mdc` — full tool contract + the audit/close workflow.
   - `workspacegraph.mdc` — guardrails so `taskagent_workspacegraph_*` is
     used for relations/impact, never to list open tasks.

It owns **no execution logic of its own**. Cursor's agent talks MCP directly
to the taskagent server; this plugin is purely the wiring.

---

## Install

### From npm

```bash
npm i -g taskagent-cursor
taskagent-cursor install --global   # write ~/.cursor/mcp.json
taskagent-cursor doctor             # verify
```

### Manual

Copy [`cursor/mcp.example.json`](./cursor/mcp.example.json) into
`~/.cursor/mcp.json` (or merge the `taskagent` entry into your existing file).

---

## CLI

| Command                                                          | Effect                                                                  |
| ---------------------------------------------------------------- | ----------------------------------------------------------------------- |
| `taskagent-cursor install [--global\|--project DIR]`      | Register the taskagent MCP server in the chosen `mcp.json`.             |
| `taskagent-cursor uninstall [--global\|--project DIR]`    | Remove the entry.                                                       |
| `taskagent-cursor deeplink [--print-scheme]`              | Print the HTTPS Add-to-Cursor install URL.                              |
| `taskagent-cursor rules [--project DIR] [--force]`        | Drop the three `.cursor/rules/*.mdc` (policy + contract + workspacegraph) into a project. |
| `taskagent-cursor doctor [--json\|--quiet]`               | Probe Cursor MCP config + HTTP server. Exit 0 ⇒ READY.                  |
| `taskagent-cursor setup`                                  | Print install hints for missing pieces.                                 |
| `taskagent-cursor marketplace`                            | Print the taskagent plugin manifest (with live deeplink baked in).         |
| `taskagent-cursor --version` / `--help`                   |                                                                         |

### Install flags

| Flag                         | Default                    | Notes                                                       |
| ---------------------------- | -------------------------- | ----------------------------------------------------------- |
| `--global` / `--project DIR` | `--global`                 | Picks which `mcp.json` to write.                            |
| `--transport http\|stdio`    | `http`                     | Cursor defaults to hosted HTTP MCP.                         |
| `--command CMD`              | (none)                     | Forces stdio fallback and overrides the binary path.        |
| `--base-url URL`             | `http://localhost:8080`    | Sets the HTTP MCP server origin.                            |
| `--token T`                  | (none)                     | Adds an Authorization header for explicit self-host config.  |
| `--name NAME`                | `taskagent`                | Rename the server entry (if you run multiple instances).    |

---

## How the deeplink flow works

```
deeplink                                               Cursor
┌─────────────────────────┐    cursor://...  ┌─────────────────────┐
│ taskagent               │ ───────────────▶ │ "Install this MCP?" │
│ [ Add to Cursor ]       │   deeplink       │ writes mcp.json     │
└─────────────────────────┘                  └─────────────────────┘
        │
        │ uses /clients/cursor-plugin/.taskagent-plugin/plugin.json
        │ from this repo (or its npm tarball)
        ▼
┌─────────────────────────┐
│ plugin manifest         │
│  name, version, deeplink│
│  install hints, rules   │
└─────────────────────────┘
```

The deeplink format is the
[official Cursor MCP install link](https://cursor.com/docs/context/mcp/install-links):

```
cursor://anysphere.cursor-deeplink/mcp/install?name=<NAME>&config=<BASE64_JSON>
```

`config` is a base64-encoded JSON object matching a single `mcpServers` entry.
Generate yours at any time with:

```bash
taskagent-cursor deeplink
# add --print-scheme only when you need the raw cursor:// URL
```

---

## Project layout

```text
clients/cursor-plugin/
├── package.json                          # npm package + CLI bin
├── bin/taskagent-cursor.mjs       # CLI entry point
├── lib/
│   ├── deeplink.mjs                      # cursor:// install link generator
│   ├── detect.mjs                        # Cursor + taskagent readiness probe
│   ├── mcp-config.mjs                    # read/write ~/.cursor/mcp.json
│   └── rules.mjs                         # drop .cursor/rules/*.mdc
├── cursor/
│   ├── mcp.example.json                  # manual install reference
│   └── rules/                            # policy + contract + workspacegraph guardrails
│       ├── taskagent-policy.mdc          # alwaysApply policy (list-first / token economy)
│       ├── taskagent.mdc                 # agent contract + audit/close workflow
│       └── workspacegraph.mdc            # graph tools: relations/impact, not inventory
├── .taskagent-plugin/plugin.json            # taskagent plugin manifest
└── tests/                                # node --test
```

---

## Requirements

- Cursor (any recent version that supports MCP)
- Node.js ≥ 20 (only for the CLI; not needed at runtime once installed)
- `taskagent-mcp` and `taskagent-server` from
  [tupical/taskagent](https://github.com/tupical/taskagent), built with
  `cargo build --release -p taskagent-server -p taskagent-mcp-bin`

---

## License

MIT — see [LICENSE](./LICENSE).
