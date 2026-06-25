#!/usr/bin/env node
// SessionStart hook: query daruma MCP server and print a compact summary
// of open tasks so Claude sees them at the top of every fresh session.
//
// Output goes to stdout — Claude Code injects it as a <system-reminder>.
// Exit 0 always so a daruma outage never blocks the session from opening.
//
// Environment variables used:
//   DARUMA_MCP_CMD   — command to start the MCP server (default: "daruma")
//   DARUMA_MCP_ARGS  — space-separated extra args (default: "mcp serve")
//   DARUMA_SCOPE     — optional project_scope path filter

import { MCPClient } from "../lib/mcp-client.mjs";

const TIMEOUT_MS = 12_000;

// How many tasks to show before truncating.
const MAX_TASKS = 15;

const STATUS_EMOJI = {
  inbox: "📥",
  todo: "⬜",
  in_progress: "🟢",
  in_review: "🔍",
  done: "✅",
  cancelled: "🚫",
};

function emoji(status) {
  return STATUS_EMOJI[status] ?? "❓";
}

function truncate(str, len) {
  if (!str) return "";
  return str.length > len ? str.slice(0, len - 1) + "…" : str;
}

function formatTask(t) {
  const e = emoji(t.status ?? "");
  const pri = t.priority ? `p${t.priority}` : "  ";
  const title = truncate(t.title ?? t.subject ?? "(no title)", 60);
  return `  ${e} [${pri}] ${title}`;
}

async function fetchSummary() {
  const cmd = process.env.DARUMA_MCP_CMD ?? "daruma";
  const extraArgs = process.env.DARUMA_MCP_ARGS ?? "mcp serve";
  const args = extraArgs.trim() ? extraArgs.trim().split(/\s+/) : ["mcp", "serve"];
  const scope = process.env.DARUMA_SCOPE;

  const client = new MCPClient();

  const timer = setTimeout(() => {
    // Force-stop on timeout; the catch below will handle the error.
    client.stop().catch(() => {});
  }, TIMEOUT_MS);

  try {
    await client.start(cmd, args, { stderrLog: null });
    await client.initialize();

    // 1. Resolve workspace / default project.
    let projectId = null;
    let projectTitle = null;
    try {
      const wsResult = await client.callTool("daruma_workspace_info", {});
      const ws = wsResult.parsed ?? {};
      projectId = ws.default_project ?? ws.defaultProject ?? null;
      projectTitle = ws.project_title ?? ws.title ?? null;
    } catch { /* no workspace info — continue without project filter */ }

    // 2. List active tasks.
    const listArgs = {
      status: ["inbox", "todo", "in_progress", "in_review"],
      limit: MAX_TASKS + 1,
    };
    if (projectId) listArgs.project_id = projectId;
    if (!projectId && scope) listArgs.project_scope = scope;

    const listResult = await client.callTool("daruma_list", listArgs);
    const raw = listResult.parsed ?? listResult.text;
    const tasks = Array.isArray(raw)
      ? raw
      : Array.isArray(raw?.tasks)
        ? raw.tasks
        : Array.isArray(raw?.items)
          ? raw.items
          : [];

    await client.stop();
    clearTimeout(timer);

    return { tasks, projectTitle, projectId };
  } catch (err) {
    clearTimeout(timer);
    try { await client.stop(); } catch { /* ignore */ }
    throw err;
  }
}

async function main() {
  let data;
  try {
    data = await fetchSummary();
  } catch (err) {
    // Soft failure: daruma unavailable — don't block session.
    const msg = err?.message ?? String(err);
    if (process.env.DARUMA_DEBUG) {
      process.stderr.write(`[daruma-claude/session-start] error: ${msg}\n`);
    }
    // Print nothing and exit cleanly so the session opens normally.
    process.exit(0);
  }

  const { tasks, projectTitle } = data;

  if (!tasks || tasks.length === 0) {
    process.stdout.write(
      "[daruma] No open tasks — run /daruma-claude:tasks to verify or /daruma-claude:start \"<goal>\" to create one.\n"
    );
    process.exit(0);
  }

  const shown = tasks.slice(0, MAX_TASKS);
  const extra = tasks.length > MAX_TASKS ? tasks.length - MAX_TASKS : 0;

  const header = projectTitle
    ? `[daruma] ${tasks.length > MAX_TASKS ? MAX_TASKS + "+" : tasks.length} open task${tasks.length !== 1 ? "s" : ""} in "${projectTitle}":`
    : `[daruma] ${tasks.length > MAX_TASKS ? MAX_TASKS + "+" : tasks.length} open task${tasks.length !== 1 ? "s" : ""}:`;

  const lines = [header, ...shown.map(formatTask)];
  if (extra > 0) {
    lines.push(`  …and ${extra} more — /daruma-claude:tasks for full list`);
  }
  lines.push("→ /daruma-claude:next to claim the next task  |  /daruma-claude:status for details");

  process.stdout.write(lines.join("\n") + "\n");
  process.exit(0);
}

main();
