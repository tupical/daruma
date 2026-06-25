// Outer orchestrator for `daruma-claude start`. Pipeline:
//   1. spawn `daruma-mcp` (stdio JSON-RPC), connect MCP client.
//   2. parse phase: derive {title, description} from input; show + confirm y/n.
//   3. resolve project_id (workspace default or basename(cwd)).
//   4. seed phase:
//        - daruma_create({task: {title, description, project_id}}) → root_task_id
//        - if plan-mode: try daruma_ai_decompose(root_task_id) → subtasks
//                        create plan, attach subtasks, confirm
//   5. execute loop:
//        - single-task: omc team N:agent "<prompt>" → comment + complete
//        - plan-mode:   loop daruma_plan_next_task → omc team → comment + complete
//   6. report progress + final summary.
//
// Why MCP over direct HTTP: token discovery, env propagation, and command-shape
// invariants are already encapsulated by `daruma-mcp`. The shim is just a
// JSON-RPC ↔ /v1/commands translator with bearer auth pulled from env. Reusing
// it keeps daruma-claude code small and isolated from API churn.

import readline from "node:readline";
import { promises as fs } from "node:fs";
import { join, basename } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

import { MCPClient } from "./mcp-client.mjs";
import { runOmcTeam } from "./omc-team-runner.mjs";

const execFileAsync = promisify(execFile);
const DEFAULT_MAX_RETRIES = 2;
const DEFAULT_WORKERS = 3;
const DEFAULT_AGENT_TYPE = "claude";
const DARUMA_MCP_BIN = process.env.DARUMA_MCP_BIN ?? "daruma-mcp";

function makePrompt(input, output) {
  const rl = readline.createInterface({ input, output });
  const ask = (q) => new Promise((resolve) => rl.question(q, resolve));
  const close = () => rl.close();
  return { ask, close };
}

function startSpinner(stdout, initialLabel) {
  if (!stdout.isTTY) {
    return { stop: () => {}, setLabel: () => {} };
  }
  const frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
  const startedMs = Date.now();
  let label = initialLabel;
  let lastWidth = 0;
  let i = 0;
  const render = () => {
    const sec = Math.round((Date.now() - startedMs) / 1000);
    const line = `${frames[i++ % frames.length]} ${label} (${sec}s)`;
    const pad = Math.max(0, lastWidth - line.length);
    stdout.write(`\r${line}${" ".repeat(pad)}`);
    lastWidth = line.length;
  };
  const id = setInterval(render, 100);
  return {
    stop: () => {
      clearInterval(id);
      stdout.write(`\r${" ".repeat(lastWidth)}\r`);
    },
    setLabel: (next) => { label = next; },
  };
}

async function withSpinner(stdout, label, fn) {
  const sp = startSpinner(stdout, label);
  try { return await fn(); }
  finally { sp.stop(); }
}

async function ensureLogDir(cwd) {
  const dir = join(cwd, ".omc", "logs");
  await fs.mkdir(dir, { recursive: true });
  return dir;
}

async function currentGitBranch(cwd) {
  try {
    const { stdout } = await execFileAsync("git", ["branch", "--show-current"], { cwd });
    const branch = stdout.trim();
    return branch || null;
  } catch {
    return null;
  }
}

// Pulls a structured payload out of an MCP tool response. daruma emits JSON
// in TextContent.text; we parse best-effort and fall back to the raw text.
function payload(resp) {
  if (resp.parsed != null) return resp.parsed;
  if (resp.text) {
    try { return JSON.parse(resp.text); } catch { /* leave */ }
  }
  return null;
}

async function callOrThrow(mcp, name, args, { allowError = false } = {}) {
  const resp = await mcp.callTool(name, args);
  if (resp.isError && !allowError) {
    throw new Error(`${name} failed: ${resp.text}`);
  }
  return resp;
}

function parseTitleAndDescription(task) {
  // First non-empty line is the title (trim, cap at 200 chars for sanity);
  // the full input — including the first line — stays as description so
  // executors retain the complete context.
  const lines = task.split(/\r?\n/);
  const firstNonEmpty = lines.find((l) => l.trim() !== "") ?? task;
  const title = firstNonEmpty.trim().slice(0, 200);
  return { title, description: task.trim() };
}

async function confirm({ ask, write, message, autoYes = false }) {
  if (autoYes) {
    write(`${message} (auto-yes)`);
    return true;
  }
  const answer = (await ask(`${message} (yes/no): `)).trim().toLowerCase();
  return /^y/.test(answer);
}

// Resolve which project_id to use for new tasks/plans. Priority:
//   1. explicit --project flag (passed in as opts.projectId)
//   2. daruma_workspace_info scope matching cwd
//   3. daruma_workspace_info default_project (if its workspace matches cwd)
//   4. existing project whose title equals basename(cwd)
//   5. create a new project named basename(cwd)
async function resolveProject({ mcp, cwd, explicitProjectId, write }) {
  if (explicitProjectId) {
    write(`[project] using explicit --project ${explicitProjectId}`);
    return explicitProjectId;
  }
  const wsResp = await callOrThrow(mcp, "daruma_workspace_info", {});
  const ws = payload(wsResp) ?? {};
  const scoped = projectFromWorkspaceScopes(ws, cwd);
  if (scoped) {
    write(`[project] using scope ${scoped.scope} (${scoped.project_id})`);
    return scoped.project_id;
  }
  if (ws.default_project && ws.workspace) {
    // workspace_info reports the path the *server* sees, not necessarily cwd.
    // Use the default only when paths align; otherwise fall through to title
    // lookup so the user doesn't accidentally drop tasks into someone else's
    // workspace.
    const wsPath = String(ws.workspace).replace(/\/$/, "");
    const cwdPath = cwd.replace(/\/$/, "");
    if (wsPath === cwdPath) {
      write(`[project] using workspace default ${ws.default_project}`);
      return ws.default_project;
    }
  }

  const listResp = await callOrThrow(mcp, "daruma_project_list", {});
  const projects = payload(listResp) ?? [];
  const desired = basename(cwd) || "daruma-claude";
  const match = Array.isArray(projects)
    ? projects.find((p) => (p.title ?? "").trim().toLowerCase() === desired.toLowerCase())
    : null;
  if (match) {
    write(`[project] reusing existing project "${match.title}" (${match.id})`);
    return match.id;
  }

  const createResp = await callOrThrow(mcp, "daruma_project_create", {
    title: desired,
    description: `Auto-created by daruma-claude for ${cwd}`,
  });
  const created = payload(createResp) ?? {};
  if (!created.id) {
    throw new Error(`daruma_project_create returned no id; raw: ${createResp.text}`);
  }
  write(`[project] created new project "${desired}" (${created.id})`);
  return created.id;
}

function cleanPath(p) {
  const out = String(p ?? "").replace(/\/+$/, "");
  return out || "/";
}

function pathContains(root, path) {
  return path === root || path.startsWith(`${root}/`);
}

function projectFromWorkspaceScopes(ws, cwd) {
  const cwdPath = cleanPath(cwd);
  const scopes = Array.isArray(ws.scopes) ? ws.scopes : [];
  return scopes
    .map((scope) => ({
      scope: cleanPath(scope.scope),
      project_id: scope.project_id,
    }))
    .filter((scope) => scope.project_id && pathContains(scope.scope, cwdPath))
    .sort((a, b) => b.scope.length - a.scope.length)[0] ?? null;
}

async function createRootTask({ mcp, title, description, projectId, write }) {
  const resp = await callOrThrow(mcp, "daruma_create", {
    task: {
      title,
      description,
      project_id: projectId,
      status: "todo",
    },
  });
  const task = payload(resp) ?? {};
  if (!task.id) {
    throw new Error(`daruma_create returned no id; raw: ${resp.text}`);
  }
  write(`[task] created root task ${task.id}: ${title}`);
  return task;
}

async function commentBranch({ mcp, taskId, branch, write }) {
  if (!branch) return;
  const resp = await callOrThrow(
    mcp,
    "daruma_comment",
    { task_id: taskId, body: `branch: ${branch}` },
    { allowError: true },
  );
  if (resp.isError) {
    write(`[branch] failed to comment branch on ${taskId}: ${resp.text.slice(0, 120)}`);
  }
}

async function tryDecompose({ mcp, taskId, write }) {
  // ai_decompose returns 502 ai_unavailable when OPENAI_API_KEY isn't set on
  // the server. Treat that case as "no AI, single-task mode" without aborting.
  const resp = await mcp.callTool("daruma_ai_decompose", { task_id: taskId });
  if (resp.isError) {
    write(`[decompose] AI decomposition unavailable: ${resp.text.slice(0, 200)}`);
    return null;
  }
  const result = payload(resp) ?? {};
  const subtasks = Array.isArray(result.subtasks) ? result.subtasks
                : Array.isArray(result) ? result
                : null;
  if (!subtasks || subtasks.length === 0) {
    write(`[decompose] AI returned no subtasks; falling back to single-task mode`);
    return null;
  }
  write(`[decompose] AI produced ${subtasks.length} subtasks`);
  return subtasks;
}

async function buildPlan({ mcp, projectId, title, subtasks, write }) {
  const planResp = await callOrThrow(mcp, "daruma_plan_create", {
    title,
    project_id: projectId,
    description: `Plan for: ${title}`,
  });
  const plan = payload(planResp) ?? {};
  if (!plan.id) throw new Error(`daruma_plan_create returned no id; raw: ${planResp.text}`);
  write(`[plan] created ${plan.id}: ${title}`);

  for (let i = 0; i < subtasks.length; i++) {
    const sub = subtasks[i];
    // ai_decompose may return tasks already persisted (with `id`) or only
    // proposals (with title/description). Handle both.
    let subTaskId = sub.id ?? sub.task_id ?? null;
    if (!subTaskId) {
      const createResp = await callOrThrow(mcp, "daruma_create", {
        task: {
          title: sub.title ?? `Subtask ${i + 1}`,
          description: sub.description ?? "",
          project_id: projectId,
          status: "todo",
        },
      });
      subTaskId = payload(createResp)?.id;
      if (!subTaskId) throw new Error(`subtask create returned no id; raw: ${createResp.text}`);
    }
    await callOrThrow(mcp, "daruma_plan_add_task", {
      plan_id: plan.id,
      task_id: subTaskId,
      position: i,
    });
  }
  return plan;
}

function executePromptFor(task) {
  const title = task.title ?? task.subject ?? "Untitled task";
  const description = task.description ?? "";
  return description ? `${title}\n\n${description}` : title;
}

async function executeOnce({
  task, workers, agentType, cwd, stderrLog, stdout, write,
}) {
  const prompt = executePromptFor(task);
  const sp = startSpinner(stdout, "omc team: starting");
  let lastCountsKey = null;
  const formatCounts = (c) =>
    `omc team: ${c.completed}/${c.total} done` +
    (c.in_progress ? `, ${c.in_progress} running` : "") +
    (c.failed ? `, ${c.failed} failed` : "") +
    (c.pending ? `, ${c.pending} pending` : "");

  try {
    return await runOmcTeam({
      prompt,
      workers,
      agentType,
      cwd,
      stderrLog,
      onProgress: (e) => {
        if (e.kind === "stale_team_cleanup") {
          sp.setLabel(`omc team: cleaning up stale team "${e.staleTeam}"`);
          if (!stdout.isTTY) write(`[omc team] cleaning up stale team from previous run: ${e.staleTeam}`);
        }
        if (e.kind === "started") {
          sp.setLabel(`omc team [${e.teamName}]: initialising`);
          if (!stdout.isTTY) write(`[omc team started: ${e.teamName}]`);
        }
        if (e.kind === "counts") {
          const c = e.counts;
          const key = `${c.total}/${c.completed}/${c.failed}/${c.in_progress}/${c.pending}`;
          if (key === lastCountsKey) return;
          lastCountsKey = key;
          sp.setLabel(formatCounts(c));
          if (!stdout.isTTY) write(`[counts] ${formatCounts(c)}`);
        }
        if (e.kind === "api_error") {
          sp.setLabel(`omc team: api transient (retrying) — ${e.message.slice(0, 60)}`);
          if (!stdout.isTTY) write(`[omc api transient: ${e.message}]`);
        }
      },
    });
  } finally {
    sp.stop();
  }
}

async function executeTaskWithRetries({
  mcp, task, maxRetries, workers, agentType, cwd, stderrLog, stdout, write, branch = null,
}) {
  let lastResult = null;
  for (let attempt = 1; attempt <= maxRetries + 1; attempt++) {
    write(`\n=== task ${task.id}: attempt ${attempt}/${maxRetries + 1} — ${task.title} ===`);
    await callOrThrow(mcp, "daruma_set_status", { id: task.id, status: "in_progress" });
    if (attempt === 1) {
      await commentBranch({ mcp, taskId: task.id, branch, write });
    }
    const result = await executeOnce({
      task, workers, agentType, cwd, stderrLog, stdout, write,
    });
    lastResult = result;
    write(`[task ${task.id}] omc team result: ok=${result.ok} completed=${result.counts.completed} failed=${result.counts.failed}`);

    // Comment the artifact onto the task regardless of verdict, so the trail
    // survives even when execution fails. daruma_comment doesn't accept
    // very large bodies; truncate to a safe upper bound.
    const body = result.artifact.length > 16_000
      ? result.artifact.slice(0, 16_000) + `\n\n…(truncated ${result.artifact.length - 16_000} chars)`
      : result.artifact;
    await callOrThrow(mcp, "daruma_comment", {
      task_id: task.id,
      body: `### Attempt ${attempt} — omc team ${result.teamName}\n\n${body}`,
    }, { allowError: true });

    if (result.ok) {
      await callOrThrow(mcp, "daruma_complete", { id: task.id });
      return { ok: true, attempts: attempt, result };
    }

    if (attempt > maxRetries) break;
    write(`[task ${task.id}] failed; retrying (${attempt}/${maxRetries})`);
    await callOrThrow(mcp, "daruma_set_status", { id: task.id, status: "todo" });
  }
  return { ok: false, attempts: maxRetries + 1, result: lastResult };
}

async function runPlanLoop({
  mcp, plan, projectId, maxRetries, workers, agentType, cwd, stderrLog, stdout, write, branch = null,
}) {
  // run_id semantics in daruma: a "claim ticket" used by plan_next_task
  // to track which agent is pulling work. We don't need real run lifecycle
  // tracking for v1 — just a stable id for the duration of this invocation.
  const runId = `daruma-claude-${Date.now()}`;
  const summaries = [];
  let safetyLimit = 100; // hard cap to prevent runaway loops
  while (safetyLimit-- > 0) {
    const nextResp = await mcp.callTool("daruma_plan_next_task", {
      id: plan.id,
      run_id: runId,
    });
    if (nextResp.isError) {
      write(`[plan] next_task error: ${nextResp.text.slice(0, 200)}`);
      break;
    }
    const next = payload(nextResp);
    if (!next || (Array.isArray(next) && next.length === 0) || !next.id) {
      write(`[plan] no more eligible tasks`);
      break;
    }
    const taskOutcome = await executeTaskWithRetries({
      mcp, task: next, maxRetries, workers, agentType, cwd, stderrLog, stdout, write, branch,
    });
    summaries.push({ taskId: next.id, ...taskOutcome });
    if (!taskOutcome.ok) {
      write(`[plan] task ${next.id} exhausted retries; halting plan execution`);
      break;
    }
  }
  const planResp = await mcp.callTool("daruma_plan_get", { id: plan.id });
  const planState = payload(planResp);
  return { runId, summaries, planState };
}

export async function runDarumaStart({
  task,
  cwd = process.cwd(),
  maxRetries = DEFAULT_MAX_RETRIES,
  workers = DEFAULT_WORKERS,
  agentType = DEFAULT_AGENT_TYPE,
  planMode = false,
  projectId = null,
  autoYes = false,
  stdin = process.stdin,
  stdout = process.stdout,
} = {}) {
  if (!task || !task.trim()) throw new Error("runDarumaStart: task is required");
  if (!autoYes && !stdin.isTTY) autoYes = true;

  const logDir = await ensureLogDir(cwd);
  const mcpStderrLog = join(logDir, "daruma-mcp.stderr.log");
  const teamStderrLog = join(logDir, "omc-team.stderr.log");

  const write = (s) => { stdout.write(`${s}\n`); };
  const userPrompt = makePrompt(stdin, stdout);

  const mcp = new MCPClient();
  let sigintHandler = null;

  try {
    write(`[daruma-claude] starting daruma-mcp (logs: ${mcpStderrLog})`);
    const { darumaMcpChildEnv } = await import("./detect.mjs");
    const childEnv = await darumaMcpChildEnv();
    await mcp.start(DARUMA_MCP_BIN, [], {
      cwd,
      stderrLog: mcpStderrLog,
      env: childEnv,
    });
    await mcp.initialize();
    write(`[daruma-claude] mcp server ready: ${mcp._serverInfo?.name}@${mcp._serverInfo?.version}`);
    const branch = await currentGitBranch(cwd);
    if (branch) write(`[branch] ${branch}`);

    sigintHandler = async () => {
      write("\n[daruma-claude] SIGINT — shutting down");
      try { await mcp.stop(); } catch { /* best-effort */ }
      process.exit(130);
    };
    process.on("SIGINT", sigintHandler);

    // --- Phase 1: parse + confirm ---------------------------------------
    write(`\n=== Phase 1: Parse ===`);
    const { title, description } = parseTitleAndDescription(task);
    write(`Title:       ${title}`);
    write(`Description: ${description.length > 280 ? description.slice(0, 280) + "…" : description}`);
    if (!(await confirm({ ask: userPrompt.ask, write, message: "Proceed with this task?", autoYes }))) {
      write("Cancelled by user.");
      return { cancelled: true };
    }

    // --- Phase 2: resolve project, create root task ---------------------
    write(`\n=== Phase 2: Project + Root task ===`);
    const resolvedProjectId = await resolveProject({
      mcp, cwd, explicitProjectId: projectId, write,
    });
    const rootTask = await createRootTask({
      mcp, title, description, projectId: resolvedProjectId, write,
    });
    await commentBranch({ mcp, taskId: rootTask.id, branch, write });

    // --- Phase 3: decompose into plan (optional) ------------------------
    let plan = null;
    if (planMode) {
      write(`\n=== Phase 3: Decompose ===`);
      const subtasks = await tryDecompose({ mcp, taskId: rootTask.id, write });
      if (subtasks && subtasks.length > 0) {
        plan = await buildPlan({
          mcp, projectId: resolvedProjectId, title, subtasks, write,
        });
        if (!(await confirm({ ask: userPrompt.ask, write, message: `Execute plan with ${subtasks.length} subtasks?`, autoYes }))) {
          write("Cancelled by user.");
          return { cancelled: true, rootTaskId: rootTask.id, planId: plan.id };
        }
      } else {
        write(`[decompose] falling back to single-task execution on root task`);
      }
    }

    // --- Phase 4: execute -----------------------------------------------
    write(`\n=== Phase 4: Execute ===`);
    let outcome;
    if (plan) {
      outcome = await runPlanLoop({
        mcp, plan, projectId: resolvedProjectId, maxRetries,
        workers, agentType, cwd, stderrLog: teamStderrLog, stdout, write, branch,
      });
    } else {
      const single = await executeTaskWithRetries({
        mcp, task: rootTask, maxRetries, workers, agentType, cwd,
        stderrLog: teamStderrLog, stdout, write, branch,
      });
      outcome = { summaries: [{ taskId: rootTask.id, ...single }], planState: null };
    }

    // --- Phase 5: report -------------------------------------------------
    write(`\n=== Final ===`);
    const succeeded = outcome.summaries.filter((s) => s.ok).length;
    const failed = outcome.summaries.filter((s) => !s.ok).length;
    write(`Tasks: ${succeeded} succeeded, ${failed} failed (of ${outcome.summaries.length})`);
    if (outcome.planState) {
      const p = outcome.planState;
      write(`Plan ${p.id ?? plan?.id}: status=${p.status ?? "?"} progress=${p.progress ?? "?"}`);
    }
    write(`Root task: ${rootTask.id}`);
    return {
      ok: failed === 0,
      rootTaskId: rootTask.id,
      planId: plan?.id ?? null,
      summaries: outcome.summaries,
      planState: outcome.planState,
    };
  } finally {
    if (sigintHandler) process.off("SIGINT", sigintHandler);
    userPrompt.close();
    try { await mcp.stop(); } catch { /* server already gone */ }
  }
}

// Backwards-compat re-export. bin/omo.mjs used `runDarumaStartLegacy`; new bin will use
// runDarumaStart but we keep both names live during the transition.
export const runDarumaStartLegacy = runDarumaStart;

// Test-only exports.
export const _internal = {
  parseTitleAndDescription,
  executePromptFor,
  currentGitBranch,
  payload,
};
