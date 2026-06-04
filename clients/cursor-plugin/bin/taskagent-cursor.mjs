#!/usr/bin/env node
// `taskagent-cursor` — Cursor companion CLI for tupical/taskagent.
//
// Subcommands:
//   install [--global|--project DIR] [--command CMD] [--base-url URL] [--token T]
//                                      Register the taskagent MCP server in
//                                      Cursor's mcp.json. --global (default)
//                                      writes ~/.cursor/mcp.json; --project
//                                      writes ./.cursor/mcp.json.
//   uninstall [--global|--project DIR]
//                                      Remove the taskagent entry.
//   deeplink [--print-url] [--base-url URL] [--token T] [--command CMD]
//                                      Print the cursor:// deeplink (and the
//                                      https://cursor.com/install-mcp mirror)
//                                      that a marketplace card can render as
//                                      an "Add to Cursor" button.
//   rules [--project DIR] [--force]
//                                      Drop the bundled .cursor/rules/taskagent.mdc
//                                      into a project so Cursor's agent knows
//                                      how to drive the taskagent MCP tools.
//   doctor [--json] [--quiet]
//                                      Probe Cursor + taskagent-mcp + HTTP server.
//   setup                              Print install hints for missing pieces.
//   marketplace                        Print the taskagent marketplace manifest.
//   --version | --help

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  detectAll,
  detectCursor,
  detectTaskagent,
  formatReport,
} from "../lib/detect.mjs";
import {
  buildTaskagentInstallLinks,
  defaultTaskagentConfig,
} from "../lib/deeplink.mjs";
import {
  resolveMcpPath,
  removeServer,
  upsertServer,
} from "../lib/mcp-config.mjs";
import { installRules } from "../lib/rules.mjs";
import { installCommands } from "../lib/commands.mjs";
import { installOmcGuard, removeOmcGuard } from "../lib/omc-guard.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));
const marketplaceManifest = JSON.parse(
  readFileSync(join(__dirname, "..", ".taskagent-plugin", "plugin.json"), "utf8"),
);

const HELP = `taskagent-cursor v${pkg.version} — Cursor plugin for tupical/taskagent

Usage:
  taskagent-cursor install [--global|--project DIR] [--command CMD]
                                  [--api-url URL] [--base-url URL] [--token T]
                                  [--api prod|staging|self-host] [--name NAME]
                                  [--no-rules] [--no-omc-guard]
                                  [--rules-dir DIR] [--force]
        Register the taskagent MCP server in Cursor's mcp.json AND drop the
        bundled .cursor/rules/ + .omc/AGENTS.md guard into the project so
        Cursor's agent defaults to taskagent for tasks/plans and OMC
        skills do not author .omc/plans/.

        --global  (default) → ~/.cursor/mcp.json
        --project DIR       → <DIR>/.cursor/mcp.json (defaults to cwd)
        --rules-dir DIR     → where to drop .cursor/rules + .omc/AGENTS.md
                              (defaults to --project DIR or cwd).
        --no-rules          → skip .cursor/rules/ install.
        --no-commands       → skip .cursor/commands/ install.
        --no-omc-guard      → skip .omc/AGENTS.md guard.
        --force             → overwrite existing rules and commands.

  taskagent-cursor uninstall [--global|--project DIR] [--name NAME]
                                    [--rules-dir DIR] [--purge]
        Remove the taskagent entry from mcp.json. With --purge, also remove
        the bundled rules and the managed .omc/AGENTS.md block.

  taskagent-cursor deeplink [--api-url URL] [--base-url URL] [--token T]
                                   [--api prod|staging|self-host] [--command CMD]
                                   [--name NAME] [--print-url]
        Print the cursor:// install deeplink that a marketplace can render
        as an "Add to Cursor" button. With --print-url, also print the
        https://cursor.com/install-mcp mirror URL.

  taskagent-cursor rules [--project DIR] [--force]
        Install the bundled .cursor/rules/*.mdc files into a project.

  taskagent-cursor commands [--project DIR] [--force]
        Install the bundled .cursor/commands/*.md slash commands
        (/taskagent-tasks, /taskagent-plan, /taskagent-next,
        /taskagent-mine) into a project.

  taskagent-cursor omc-guard [--project DIR]
        Refresh the managed .omc/AGENTS.md block that tells OMC skills to
        route plans through taskagent and stay out of .omc/plans/.

  taskagent-cursor doctor [--json] [--quiet]
        Probe Cursor + taskagent-mcp + HTTP server (exit 0 = READY).

  taskagent-cursor setup
        Print install hints for missing dependencies.

  taskagent-cursor marketplace
        Print the taskagent marketplace manifest (JSON).

  taskagent-cursor --version | -v
  taskagent-cursor --help    | -h
`;

function parseScopeFlags(rest) {
  const opts = {
    scope: "global",
    projectDir: undefined,
    rulesDir: undefined,
    command: undefined,
    apiUrl: undefined,
    baseUrl: undefined,
    remote: undefined,
    token: undefined,
    name: "taskagent",
    force: false,
    printUrl: false,
    json: false,
    quiet: false,
    noRules: false,
    noCommands: false,
    noOmcGuard: false,
    purge: false,
  };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    switch (a) {
      case "--global":
        opts.scope = "global"; break;
      case "--project":
        opts.scope = "project";
        if (rest[i + 1] && !rest[i + 1].startsWith("--")) {
          opts.projectDir = rest[++i];
        }
        break;
      case "--rules-dir":
        opts.rulesDir = requireValue(a, rest[++i]); break;
      case "--command":
        opts.command = requireValue(a, rest[++i]); break;
      case "--api-url":
        opts.apiUrl = requireValue(a, rest[++i]); break;
      case "--base-url":
        opts.baseUrl = requireValue(a, rest[++i]); break;
      case "--api":
        opts.remote = requireValue(a, rest[++i]); break;
      case "--token":
        opts.token = requireValue(a, rest[++i]); break;
      case "--name":
        opts.name = requireValue(a, rest[++i]); break;
      case "--force":
      case "-f":
        opts.force = true; break;
      case "--print-url":
        opts.printUrl = true; break;
      case "--json":
        opts.json = true; break;
      case "--quiet":
        opts.quiet = true; break;
      case "--no-rules":
        opts.noRules = true; break;
      case "--no-commands":
        opts.noCommands = true; break;
      case "--no-omc-guard":
        opts.noOmcGuard = true; break;
      case "--purge":
        opts.purge = true; break;
      default:
        throw new Error(`unknown flag: ${a}`);
    }
  }
  if (opts.json && opts.quiet) {
    throw new Error("--json and --quiet are mutually exclusive");
  }
  return opts;
}

function resolveRulesDir(opts) {
  return opts.rulesDir ?? opts.projectDir ?? process.cwd();
}

const RULES_VERB = {
  installed: "Installed",
  overwritten: "Overwrote",
  "skipped-exists": "Already exists (use --force to overwrite)",
};

const COMMANDS_VERB = RULES_VERB;

const OMC_VERB = {
  installed: "Created",
  updated: "Refreshed",
  appended: "Appended block to",
  unchanged: "Already current",
  "skipped-no-omc": "No .omc/ detected, skipped",
  "removed-block": "Removed managed block from",
  "removed-file": "Removed",
  missing: "Nothing to remove at",
};

function requireValue(flag, value) {
  if (!value || value.startsWith("--")) {
    throw new Error(`${flag} requires a value`);
  }
  return value;
}

function installEnvOpts(opts) {
  return {
    command: opts.command,
    apiUrl: opts.apiUrl ?? opts.baseUrl,
    token: opts.token,
    remote: opts.remote,
  };
}

async function cmdInstall(rest) {
  const opts = parseScopeFlags(rest);
  const path = resolveMcpPath({ scope: opts.scope, projectDir: opts.projectDir });
  const entry = await defaultTaskagentConfig(installEnvOpts(opts));
  const result = await upsertServer(path, opts.name, entry);
  const verb = {
    added: "Added",
    replaced: "Replaced",
    unchanged: "Already present (unchanged)",
  }[result.action] ?? result.action;
  process.stdout.write(`${verb} ${opts.name} in ${result.path}\n`);
  process.stdout.write(JSON.stringify(entry, null, 2) + "\n");

  const rulesDir = resolveRulesDir(opts);

  if (!opts.noRules) {
    const rulesResults = await installRules({
      projectDir: rulesDir,
      overwrite: opts.force,
    });
    process.stdout.write("\nCursor rules (default-tracker policy):\n");
    for (const r of rulesResults) {
      const v = RULES_VERB[r.action] ?? r.action;
      process.stdout.write(`  ${v}: ${r.path}\n`);
    }
  }

  if (!opts.noCommands) {
    const cmdResults = await installCommands({
      projectDir: rulesDir,
      overwrite: opts.force,
    });
    process.stdout.write("\nCursor slash commands:\n");
    for (const r of cmdResults) {
      const v = COMMANDS_VERB[r.action] ?? r.action;
      process.stdout.write(`  ${v}: ${r.path}\n`);
    }
  }

  if (!opts.noOmcGuard) {
    const guard = await installOmcGuard({ projectDir: rulesDir });
    const v = OMC_VERB[guard.action] ?? guard.action;
    process.stdout.write(`\nOMC guard: ${v} ${guard.path}\n`);
    if (guard.action === "skipped-no-omc") {
      process.stdout.write(
        "  (no oh-my-claudecode artifacts in this project — nothing to override)\n",
      );
    }
  }

  process.stdout.write("\nRestart Cursor (or reload the MCP panel) to pick up the change.\n");
}

async function cmdUninstall(rest) {
  const opts = parseScopeFlags(rest);
  const path = resolveMcpPath({ scope: opts.scope, projectDir: opts.projectDir });
  const result = await removeServer(path, opts.name);
  if (result.action === "removed") {
    process.stdout.write(`Removed ${opts.name} from ${result.path}\n`);
  } else {
    process.stdout.write(`No ${opts.name} entry in ${result.path}\n`);
  }
  if (opts.purge) {
    const rulesDir = resolveRulesDir(opts);
    const guard = await removeOmcGuard({ projectDir: rulesDir });
    const v = OMC_VERB[guard.action] ?? guard.action;
    process.stdout.write(`OMC guard: ${v} ${guard.path}\n`);
  }
}

async function cmdDeeplink(rest) {
  const opts = parseScopeFlags(rest);
  const { deeplink, httpsUrl, config } = await buildTaskagentInstallLinks({
    name: opts.name,
    ...installEnvOpts(opts),
  });
  process.stdout.write(deeplink + "\n");
  if (opts.printUrl) {
    process.stdout.write(httpsUrl + "\n");
  }
  if (process.env.TASKAGENT_DEBUG) {
    process.stderr.write("\nencoded config:\n");
    process.stderr.write(JSON.stringify(config, null, 2) + "\n");
  }
}

async function cmdRules(rest) {
  const opts = parseScopeFlags(rest);
  const dir = resolveRulesDir(opts);
  const results = await installRules({ projectDir: dir, overwrite: opts.force });
  for (const r of results) {
    const verb = RULES_VERB[r.action] ?? r.action;
    process.stdout.write(`${verb}: ${r.path}\n`);
  }
}

async function cmdCommands(rest) {
  const opts = parseScopeFlags(rest);
  const dir = resolveRulesDir(opts);
  const results = await installCommands({ projectDir: dir, overwrite: opts.force });
  for (const r of results) {
    const verb = COMMANDS_VERB[r.action] ?? r.action;
    process.stdout.write(`${verb}: ${r.path}\n`);
  }
}

async function cmdOmcGuard(rest) {
  const opts = parseScopeFlags(rest);
  const dir = resolveRulesDir(opts);
  const result = await installOmcGuard({ projectDir: dir });
  const verb = OMC_VERB[result.action] ?? result.action;
  process.stdout.write(`${verb}: ${result.path}\n`);
}

async function cmdDoctor(rest) {
  const opts = parseScopeFlags(rest);
  const report = await detectAll({ projectDir: opts.projectDir });
  if (opts.json) {
    process.stdout.write(JSON.stringify({
      ready: report.ready,
      cursor: {
        installed: report.cursor.installed,
        cli: report.cursor.cli,
      },
      taskagent: {
        installed: report.taskagent.installed,
        mcpReady: report.taskagent.mcpReady,
        cli: report.taskagent.cli,
        http: report.taskagent.http,
        cursorMcp: report.taskagent.cursorMcp,
        projectRules: report.taskagent.projectRules,
        projectCommands: report.taskagent.projectCommands,
      },
      omc: report.omc,
    }) + "\n");
  } else if (!opts.quiet) {
    process.stdout.write(formatReport(report) + "\n");
  }
  process.exit(report.ready ? 0 : 1);
}

async function cmdSetup() {
  const report = await detectAll();
  if (report.ready) {
    process.stdout.write("Cursor + taskagent are ready. Nothing to install.\n");
    process.stdout.write(formatReport(report) + "\n");
    return;
  }
  process.stdout.write("Install the missing pieces below, then re-run `taskagent-cursor doctor`.\n\n");
  for (const tool of [report.cursor, report.taskagent]) {
    if (tool.installed && tool.mcpReady !== false) continue;
    const hint = tool.installed ? tool.mcpHint : tool.installHint;
    process.stdout.write(`# ${tool.name}\n${hint}\n\n`);
  }
}

async function cmdMarketplace() {
  // The taskagent marketplace consumer reads this verbatim. We embed the live
  // deeplink so the manifest never drifts from the actual install button.
  const links = await buildTaskagentInstallLinks({ remote: "prod" });
  const augmented = {
    ...marketplaceManifest,
    install: {
      ...marketplaceManifest.install,
      cursorDeeplink: links.deeplink,
      cursorHttpsUrl: links.httpsUrl,
    },
  };
  process.stdout.write(JSON.stringify(augmented, null, 2) + "\n");
}

async function main(argv) {
  const [, , cmd, ...rest] = argv;
  switch (cmd) {
    case undefined:
    case "--help":
    case "-h":
    case "help":
      process.stdout.write(HELP);
      return;
    case "--version":
    case "-v":
      process.stdout.write(pkg.version + "\n");
      return;
    case "install":
      return cmdInstall(rest);
    case "uninstall":
      return cmdUninstall(rest);
    case "deeplink":
      return cmdDeeplink(rest);
    case "rules":
      return cmdRules(rest);
    case "commands":
      return cmdCommands(rest);
    case "omc-guard":
      return cmdOmcGuard(rest);
    case "doctor":
      return cmdDoctor(rest);
    case "setup":
      return cmdSetup();
    case "marketplace":
      return await cmdMarketplace();
    default:
      process.stderr.write(`Unknown command: ${cmd}\n\n${HELP}`);
      process.exit(2);
  }
}

main(process.argv).catch((err) => {
  process.stderr.write(`taskagent-cursor: ${err.message ?? err}\n`);
  if (process.env.TASKAGENT_DEBUG) process.stderr.write(`${err.stack}\n`);
  process.exit(1);
});
