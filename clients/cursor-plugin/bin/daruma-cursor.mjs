#!/usr/bin/env node
// `daruma-cursor` — Cursor companion CLI for tupical/daruma.
//
// Subcommands:
//   install [--global|--project DIR] [--transport http|stdio] [--command CMD]
//                                      [--base-url URL] [--token T]
//                                      Register the daruma MCP server in
//                                      Cursor's mcp.json. --global (default)
//                                      writes ~/.cursor/mcp.json; --project
//                                      writes ./.cursor/mcp.json.
//   uninstall [--global|--project DIR]
//                                      Remove the daruma entry.
//   deeplink [--base-url URL] [--token T] [--command CMD]
//                                      Print the official cursor:// MCP install
//                                      URL for an "Add to Cursor" button.
//   rules [--project DIR] [--force]
//                                      Drop the bundled .cursor/rules/daruma.mdc
//                                      into a project so Cursor's agent knows
//                                      how to drive the daruma MCP tools.
//   doctor [--json] [--quiet]
//                                      Probe Cursor + daruma binary + HTTP server.
//   mode [off|lite|full]               Show or set the intake strictness mode
//                                      (~/.daruma/mode, shared across daruma
//                                      clients). No arg → show current mode.
//   setup                              Print install hints for missing pieces.
//   marketplace                        Print the daruma marketplace manifest.
//   --version | --help

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  detectAll,
  formatReport,
} from "../lib/detect.mjs";
import {
  buildDarumaInstallLinks,
  defaultDarumaConfig,
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
import { MODES, readMode, writeMode } from "../lib/mode.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(__dirname, "..", "package.json"), "utf8"));
const marketplaceManifest = JSON.parse(
  readFileSync(join(__dirname, "..", ".daruma-plugin", "plugin.json"), "utf8"),
);

const HELP = `daruma-cursor v${pkg.version} — Cursor plugin for tupical/daruma

Usage:
  daruma-cursor install [--global|--project DIR] [--transport http|stdio]
                                  [--command CMD]
                                  [--api-url URL] [--base-url URL] [--token T]
                                  [--api prod|self-host] [--name NAME]
                                  [--no-rules] [--commands] [--no-omc-guard]
                                  [--rules-dir DIR] [--force]
        Register the daruma MCP server in Cursor's mcp.json AND drop the
        bundled .cursor/rules/ into the selected scope so Cursor's agent
        defaults to daruma for tasks/plans and OMC skills do not author
        .omc/plans/.

        Slash commands (/daruma-tasks, /daruma-plan, ...) now ship FROM the
        daruma MCP server as prompts, so install no longer copies
        .cursor/commands/ by default. Pass --commands to also drop the local
        copies (for MCP clients that don't surface server prompts).

        The MCP entry is written ONLY if the server is not already registered.
        A server already installed (e.g. via the one-click OAuth deeplink) is
        kept untouched; pass --force to overwrite it.

        --global  (default) → ~/.cursor/{mcp.json,rules}
        --project DIR       → <DIR>/.cursor/{mcp.json,rules}
        --rules-dir DIR     → where to drop .cursor/rules + .omc/AGENTS.md
                              (relative paths resolve from home for --global,
                              cwd for --project).
        --no-rules          → skip .cursor/rules/ install.
        --commands          → also install local .cursor/commands/ copies.
        --no-omc-guard      → skip .omc/AGENTS.md guard.
        --force             → overwrite existing mcp.json entry, rules,
                              and commands.

  daruma-cursor uninstall [--global|--project DIR] [--name NAME]
                                    [--rules-dir DIR] [--purge]
        Remove the daruma entry from mcp.json. With --purge, also remove
        the bundled rules and the managed .omc/AGENTS.md block.

  daruma-cursor deeplink [--api-url URL] [--base-url URL] [--token T]
                                   [--api prod|self-host]
                                   [--transport http|stdio] [--command CMD]
                                   [--name NAME] [--print-scheme]
        Print the official cursor:// URL that a browser or marketplace can
        render as an "Add to Cursor" button.

  daruma-cursor rules [--project DIR] [--force]
        Install the bundled .cursor/rules/*.mdc files into a project.

  daruma-cursor commands [--project DIR] [--force]
        Install the bundled .cursor/commands/*.md slash commands
        (/daruma-tasks, /daruma-plan, /daruma-next,
        /daruma-mine, /daruma-mode) into a project.

  daruma-cursor omc-guard [--project DIR]
        Refresh the managed .omc/AGENTS.md block that tells OMC skills to
        route plans through daruma and stay out of .omc/plans/.

  daruma-cursor doctor [--json] [--quiet]
        Probe Cursor + daruma binary + HTTP server (exit 0 = READY).

  daruma-cursor mode [off|lite|full]
        Show or set the intake strictness mode: how aggressively raw
        input gets decomposed into a plan via daruma_plan_materialize
        before becoming a task. No arg (or --show) prints the current
        mode. Persisted to ~/.daruma/mode, shared across daruma clients.

  daruma-cursor setup
        Print install hints for missing dependencies.

  daruma-cursor marketplace
        Print the daruma marketplace manifest (JSON).

  daruma-cursor --version | -v
  daruma-cursor --help    | -h
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
    name: "daruma",
    force: false,
    printScheme: false,
    json: false,
    quiet: false,
    noRules: false,
    commands: false,
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
      case "--print-scheme":
        opts.printScheme = true; break;
      case "--json":
        opts.json = true; break;
      case "--quiet":
        opts.quiet = true; break;
      case "--no-rules":
        opts.noRules = true; break;
      case "--commands":
        opts.commands = true; break;
      case "--no-commands":
        break; // deprecated: commands are no longer installed by default
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
  const ui = createCliUi({ title: "Daruma Cursor Installer" });
  const opts = parseScopeFlags(rest);
  ui.header();

  const path = resolveMcpPath({ scope: opts.scope, projectDir: opts.projectDir });
  const { entry, result } = await ui.task(
    "Registering Cursor MCP server...",
    async () => {
      const entry = await defaultDarumaConfig(installEnvOpts(opts));
      const result = await upsertServer(path, opts.name, entry, { overwrite: opts.force });
      return { entry, result };
    },
    "Cursor MCP server registered",
  );
  const verb = {
    added: "Added",
    replaced: "Replaced",
    kept: "Already installed — kept existing entry (use --force to overwrite)",
    unchanged: "Already present (unchanged)",
  }[result.action] ?? result.action;
  ui.detail(`  ${verb} ${opts.name} in ${result.path}`);
  // When we kept an existing entry (e.g. a one-click OAuth install), show what
  // is actually on disk, not the default we would have written.
  ui.detail(JSON.stringify(result.after ?? entry, null, 2).split("\n").map((ln) => `  ${ln}`).join("\n"));

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

  if (opts.commands) {
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
  } else {
    ui.section("Cursor slash commands");
    ui.detail("  Shipped by the daruma MCP server as prompts — run `daruma-cursor commands` to also drop local .cursor/commands/*.");
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
  const ui = createCliUi({ title: "Daruma Cursor Uninstaller" });
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
  const { deeplink, config } = await buildDarumaInstallLinks({
    name: opts.name,
    ...installEnvOpts(opts),
  });
  process.stdout.write(deeplink + "\n");
  if (process.env.DARUMA_DEBUG) {
    process.stderr.write("\nencoded config:\n");
    process.stderr.write(JSON.stringify(config, null, 2) + "\n");
  }
}

async function cmdRules(rest) {
  const ui = createCliUi({ title: "Daruma Cursor Rules" });
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
  const ui = createCliUi({ title: "Daruma Cursor Commands" });
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
  const ui = createCliUi({ title: "Daruma OMC Guard" });
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
      daruma: {
        installed: report.daruma.installed,
        mcpReady: report.daruma.mcpReady,
        cli: report.daruma.cli,
        http: report.daruma.http,
        cursorMcp: report.daruma.cursorMcp,
        projectRules: report.daruma.projectRules,
        projectCommands: report.daruma.projectCommands,
      },
      omc: report.omc,
    }) + "\n");
  } else if (!opts.quiet) {
    process.stdout.write(formatReport(report) + "\n");
  }
  process.exit(report.ready ? 0 : 1);
}

async function cmdMode(rest) {
  const [arg] = rest;
  if (!arg || arg === "--show") {
    process.stdout.write(`daruma intake mode: ${readMode()} (${MODES.join(" | ")})\n`);
    return;
  }
  const mode = await writeMode(arg);
  process.stdout.write(`✓ daruma intake mode: ${mode}\n`);
}

async function cmdSetup() {
  const ui = createCliUi({ title: "Daruma Cursor Setup" });
  ui.header();
  const report = await detectAll();
  if (report.ready) {
    ui.success("Cursor + daruma are ready. Nothing to install.");
    process.stdout.write(formatReport(report) + "\n");
    return;
  }
  ui.warn("Install the missing pieces below, then re-run `daruma-cursor doctor`.");
  process.stdout.write("\n");
  for (const tool of [report.cursor, report.daruma]) {
    if (tool.installed && tool.mcpReady !== false) continue;
    const hint = tool.installed ? tool.mcpHint : tool.installHint;
    ui.section(tool.name);
    process.stdout.write(`${hint}\n`);
  }
}

async function cmdMarketplace() {
  // The daruma marketplace consumer reads this verbatim. We embed the live
  // deeplink so the manifest never drifts from the actual install button.
  const links = await buildDarumaInstallLinks({ remote: "prod" });
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
    case "mode":
      return cmdMode(rest);
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
  process.stderr.write(`daruma-cursor: ${err.message ?? err}\n`);
  if (process.env.DARUMA_DEBUG) process.stderr.write(`${err.stack}\n`);
  process.exit(1);
});
