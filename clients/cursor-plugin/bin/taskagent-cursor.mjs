#!/usr/bin/env node
// `taskagent-cursor` — Cursor companion CLI for tupical/taskagent.
//
// Subcommands:
//   install [--global|--project DIR] [--transport http|stdio] [--command CMD]
//                                      [--base-url URL] [--token T]
//                                      Register the taskagent MCP server in
//                                      Cursor's mcp.json. --global (default)
//                                      writes ~/.cursor/mcp.json; --project
//                                      writes ./.cursor/mcp.json.
//   uninstall [--global|--project DIR]
//                                      Remove the taskagent entry.
//   deeplink [--print-scheme] [--base-url URL] [--token T] [--command CMD]
//                                      Print the https://cursor.com/install-mcp
//                                      URL that a marketplace card can render
//                                      as an "Add to Cursor" button.
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
import { resolveCursorAssetRoot } from "../lib/paths.mjs";
import { createCliUi } from "../lib/cli-ui.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));
const marketplaceManifest = JSON.parse(
  readFileSync(join(__dirname, "..", ".taskagent-plugin", "plugin.json"), "utf8"),
);

const HELP = `taskagent-cursor v${pkg.version} — Cursor plugin for tupical/taskagent

Usage:
  taskagent-cursor install [--global|--project DIR] [--transport http|stdio]
                                  [--command CMD]
                                  [--api-url URL] [--base-url URL] [--token T]
                                  [--api prod|staging|self-host] [--name NAME]
                                  [--no-rules] [--no-omc-guard]
                                  [--rules-dir DIR] [--force]
        Register the taskagent MCP server in Cursor's mcp.json AND drop the
        bundled .cursor/rules/ + .cursor/commands/ into the selected scope so
        Cursor's agent defaults to taskagent for tasks/plans and OMC
        skills do not author .omc/plans/.

        --global  (default) → ~/.cursor/{mcp.json,rules,commands}
        --project DIR       → <DIR>/.cursor/{mcp.json,rules,commands}
        --rules-dir DIR     → where to drop .cursor/rules + .omc/AGENTS.md
                              (relative paths resolve from home for --global,
                              cwd for --project).
        --no-rules          → skip .cursor/rules/ install.
        --no-commands       → skip .cursor/commands/ install.
        --no-omc-guard      → skip .omc/AGENTS.md guard.
        --force             → overwrite existing rules and commands.

  taskagent-cursor uninstall [--global|--project DIR] [--name NAME]
                                    [--rules-dir DIR] [--purge]
        Remove the taskagent entry from mcp.json. With --purge, also remove
        the bundled rules and the managed .omc/AGENTS.md block.

  taskagent-cursor deeplink [--api-url URL] [--base-url URL] [--token T]
                                   [--api prod|staging|self-host]
                                   [--transport http|stdio] [--command CMD]
                                   [--name NAME] [--print-scheme]
        Print the https://cursor.com/install-mcp URL that a browser or
        marketplace can render as an "Add to Cursor" button. With
        --print-scheme, also print the raw cursor:// deeplink.

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
    transport: undefined,
    name: "taskagent",
    force: false,
    printScheme: false,
    json: false,
    quiet: false,
    noRules: false,
    noCommands: false,
    noOmcGuard: false,
    purge: false,
    scopeExplicit: false,
  };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    switch (a) {
      case "--global":
        opts.scope = "global";
        opts.scopeExplicit = true;
        break;
      case "--project":
        opts.scope = "project";
        opts.scopeExplicit = true;
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
      case "--transport":
        opts.transport = requireValue(a, rest[++i]); break;
      case "--name":
        opts.name = requireValue(a, rest[++i]); break;
      case "--force":
      case "-f":
        opts.force = true; break;
      case "--print-url":
      case "--print-scheme":
        opts.printScheme = true; break;
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
  return resolveCursorAssetRoot({
    scope: opts.scope,
    projectDir: opts.projectDir,
    rulesDir: opts.rulesDir,
  });
}

function projectDefaultOpts(opts) {
  if (opts.scopeExplicit || opts.rulesDir) return opts;
  return { ...opts, scope: "project" };
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
    transport: opts.transport,
  };
}

function actionKind(action) {
  if (action === "installed" || action === "overwritten" || action === "added" || action === "replaced" || action === "removed") {
    return "ok";
  }
  if (action === "unchanged" || action === "skipped-exists" || action === "skipped-no-omc" || action === "missing") {
    return "warn";
  }
  return "dot";
}

async function cmdInstall(rest) {
  const ui = createCliUi({ title: "TaskAgent Cursor Installer" });
  const opts = parseScopeFlags(rest);
  ui.header();

  const path = resolveMcpPath({ scope: opts.scope, projectDir: opts.projectDir });
  const { entry, result } = await ui.task(
    "Registering Cursor MCP server...",
    async () => {
      const entry = await defaultTaskagentConfig(installEnvOpts(opts));
      const result = await upsertServer(path, opts.name, entry);
      return { entry, result };
    },
    "Cursor MCP server registered",
  );
  const verb = {
    added: "Added",
    replaced: "Replaced",
    unchanged: "Already present (unchanged)",
  }[result.action] ?? result.action;
  ui.detail(`  ${verb} ${opts.name} in ${result.path}`);
  ui.detail(JSON.stringify(entry, null, 2).split("\n").map((ln) => `  ${ln}`).join("\n"));

  const rulesDir = resolveRulesDir(opts);

  if (!opts.noRules) {
    const rulesResults = await ui.task(
      "Installing Cursor rules...",
      () => installRules({
        projectDir: rulesDir,
        overwrite: opts.force,
      }),
      "Cursor rules ready",
    );
    ui.section("Cursor rules");
    for (const r of rulesResults) {
      const v = RULES_VERB[r.action] ?? r.action;
      ui.item(`${v}: ${r.path}`, { kind: actionKind(r.action) });
    }
  }

  if (!opts.noCommands) {
    const cmdResults = await ui.task(
      "Installing Cursor slash commands...",
      () => installCommands({
        projectDir: rulesDir,
        overwrite: opts.force,
      }),
      "Cursor slash commands ready",
    );
    ui.section("Cursor slash commands");
    for (const r of cmdResults) {
      const v = COMMANDS_VERB[r.action] ?? r.action;
      ui.item(`${v}: ${r.path}`, { kind: actionKind(r.action) });
    }
  }

  if (!opts.noOmcGuard) {
    const guard = await ui.task(
      "Refreshing OMC guard...",
      () => installOmcGuard({ projectDir: rulesDir }),
      "OMC guard checked",
    );
    const v = OMC_VERB[guard.action] ?? guard.action;
    ui.section("OMC guard");
    ui.item(`${v} ${guard.path}`, { kind: actionKind(guard.action) });
    if (guard.action === "skipped-no-omc") {
      ui.detail("  no oh-my-claudecode artifacts in this project; nothing to override");
    }
  }

  ui.success("Installation complete");
  ui.detail("  Restart Cursor or reload the MCP panel to pick up the change.");
}

async function cmdUninstall(rest) {
  const ui = createCliUi({ title: "TaskAgent Cursor Uninstaller" });
  const opts = parseScopeFlags(rest);
  ui.header();
  const path = resolveMcpPath({ scope: opts.scope, projectDir: opts.projectDir });
  const result = await ui.task(
    "Removing Cursor MCP server...",
    () => removeServer(path, opts.name),
    "Cursor MCP config checked",
  );
  if (result.action === "removed") {
    ui.success(`Removed ${opts.name} from ${result.path}`);
  } else {
    ui.warn(`No ${opts.name} entry in ${result.path}`);
  }
  if (opts.purge) {
    const rulesDir = resolveRulesDir(opts);
    const guard = await ui.task(
      "Removing managed OMC guard...",
      () => removeOmcGuard({ projectDir: rulesDir }),
      "OMC guard checked",
    );
    const v = OMC_VERB[guard.action] ?? guard.action;
    ui.item(`${v} ${guard.path}`, { kind: actionKind(guard.action) });
  }
}

async function cmdDeeplink(rest) {
  const opts = parseScopeFlags(rest);
  const { deeplink, httpsUrl, config } = await buildTaskagentInstallLinks({
    name: opts.name,
    ...installEnvOpts(opts),
  });
  process.stdout.write(httpsUrl + "\n");
  if (opts.printScheme) {
    process.stdout.write(deeplink + "\n");
  }
  if (process.env.TASKAGENT_DEBUG) {
    process.stderr.write("\nencoded config:\n");
    process.stderr.write(JSON.stringify(config, null, 2) + "\n");
  }
}

async function cmdRules(rest) {
  const ui = createCliUi({ title: "TaskAgent Cursor Rules" });
  const opts = projectDefaultOpts(parseScopeFlags(rest));
  ui.header();
  const dir = resolveRulesDir(opts);
  const results = await ui.task(
    "Installing Cursor rules...",
    () => installRules({ projectDir: dir, overwrite: opts.force }),
    "Cursor rules ready",
  );
  for (const r of results) {
    const verb = RULES_VERB[r.action] ?? r.action;
    ui.item(`${verb}: ${r.path}`, { kind: actionKind(r.action) });
  }
}

async function cmdCommands(rest) {
  const ui = createCliUi({ title: "TaskAgent Cursor Commands" });
  const opts = projectDefaultOpts(parseScopeFlags(rest));
  ui.header();
  const dir = resolveRulesDir(opts);
  const results = await ui.task(
    "Installing Cursor slash commands...",
    () => installCommands({ projectDir: dir, overwrite: opts.force }),
    "Cursor slash commands ready",
  );
  for (const r of results) {
    const verb = COMMANDS_VERB[r.action] ?? r.action;
    ui.item(`${verb}: ${r.path}`, { kind: actionKind(r.action) });
  }
}

async function cmdOmcGuard(rest) {
  const ui = createCliUi({ title: "TaskAgent OMC Guard" });
  const opts = projectDefaultOpts(parseScopeFlags(rest));
  ui.header();
  const dir = resolveRulesDir(opts);
  const result = await ui.task(
    "Refreshing OMC guard...",
    () => installOmcGuard({ projectDir: dir }),
    "OMC guard checked",
  );
  const verb = OMC_VERB[result.action] ?? result.action;
  ui.item(`${verb}: ${result.path}`, { kind: actionKind(result.action) });
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
  const ui = createCliUi({ title: "TaskAgent Cursor Setup" });
  ui.header();
  const report = await detectAll();
  if (report.ready) {
    ui.success("Cursor + taskagent are ready. Nothing to install.");
    process.stdout.write(formatReport(report) + "\n");
    return;
  }
  ui.warn("Install the missing pieces below, then re-run `taskagent-cursor doctor`.");
  process.stdout.write("\n");
  for (const tool of [report.cursor, report.taskagent]) {
    if (tool.installed && tool.mcpReady !== false) continue;
    const hint = tool.installed ? tool.mcpHint : tool.installHint;
    ui.section(tool.name);
    process.stdout.write(`${hint}\n`);
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
