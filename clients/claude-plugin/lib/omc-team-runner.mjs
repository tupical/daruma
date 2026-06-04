// omc team runner — spawns `omc team`, polls its API for completion, and
// returns a single artifact object suitable for handing to ouroboros_evaluate.
//
// Architecture context: in the new omo flow, ouroboros runs as a top-level
// MCP server and we drive interview/seed/evaluate through MCP. Execute step
// is NOT delegated to ouroboros; instead, omo invokes `omc team` here, waits
// for it to finish, and feeds the resulting summary back to
// `ouroboros_evaluate` as the artifact text. That keeps the heavy executor
// out of any nested-claude path and uses ouroboros only for what it's good
// at (requirements + judgment).
//
// `omc team` (start) returns immediately after initializing tmux workers — it
// does NOT block until completion. We poll `omc team api get-summary` until
// `pending + in_progress + blocked === 0`, then `omc team api list-tasks` for
// per-task results.

import { execFile, spawn } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import { homedir } from "node:os";

const exec = promisify(execFile);

const DEFAULT_POLL_INTERVAL_MS = 5_000;
const DEFAULT_MAX_WAIT_MS = 30 * 60 * 1000;
const DEFAULT_WORKERS = 3;
const DEFAULT_AGENT_TYPE = "claude";

// Slug rule mirrors omc's own derivation (kebab-case, lowercase, alnum+dash).
// We don't need to be byte-identical — `omc team` echoes the canonical name on
// stdout — but a close approximation lets us locate the team's state dir as a
// secondary lookup if --json output is suppressed.
function slugifyTaskName(prompt) {
  return prompt
    .toLowerCase()
    .replace(/[^a-z0-9\s-]/g, "")
    .trim()
    .split(/\s+/)
    .slice(0, 6)
    .join("-")
    .slice(0, 40)
    || "omo-team";
}

async function omcApi(operation, input, { cwd, timeoutMs = 15_000 } = {}) {
  const args = ["team", "api", operation, "--input", JSON.stringify(input), "--json"];
  const { stdout } = await exec("omc", args, { cwd, timeout: timeoutMs, maxBuffer: 8 * 1024 * 1024 });
  const trimmed = stdout.trim();
  if (!trimmed) {
    throw new Error(`omc team api ${operation} returned empty stdout`);
  }
  let parsed;
  try {
    parsed = JSON.parse(trimmed);
  } catch (err) {
    throw new Error(`omc team api ${operation} returned non-JSON: ${trimmed.slice(0, 200)}`);
  }
  if (parsed?.ok === false) {
    const code = parsed?.error?.code ?? "unknown";
    const msg = parsed?.error?.message ?? "no message";
    throw new Error(`omc team api ${operation} failed: [${code}] ${msg}`);
  }
  return parsed;
}

// Resolve teamName from `.omc/state/team/`. omc creates this directory at
// startup (cli/commands/team.js:340 calls slugifyTask + mkdir), so the
// directory name IS the canonical teamName — no parsing of human stdout
// required. We replicate omc's slug rule as the strong-match check, then
// rank by mtime when our slug rule diverges from omc's exact implementation.
async function resolveTeamNameFromStateDir({ cwd, prompt, spawnStartMs }) {
  const teamRoot = `${cwd}/.omc/state/team`;
  let entries;
  try {
    entries = await fs.readdir(teamRoot, { withFileTypes: true });
  } catch {
    return null;
  }
  const candidateSlug = slugifyTaskName(prompt);
  let freshestName = null;
  let freshestMtime = -Infinity;
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const dirPath = `${teamRoot}/${entry.name}`;
    let st;
    try { st = await fs.stat(dirPath); } catch { continue; }
    const mtimeMs = st.mtimeMs;
    // Skip dirs from previous runs (only fresh-after-spawn count).
    if (mtimeMs + 5_000 < spawnStartMs) continue;
    if (entry.name === candidateSlug) return entry.name;
    if (mtimeMs > freshestMtime) {
      freshestMtime = mtimeMs;
      freshestName = entry.name;
    }
  }
  return freshestName;
}

// Single spawn attempt; returns {teamName} on success or throws an Error
// whose `.cause` carries `{kind: "stale_leader", staleTeam}` when omc rejects
// the spawn because of one_team_per_leader_session.
async function spawnTeamOnce({ prompt, workers, agentType, cwd, stderrLog }) {
  const spawnStartMs = Date.now();
  return new Promise((resolve, reject) => {
    const args = ["team", `${workers}:${agentType}`, prompt];
    const child = spawn("omc", args, { cwd, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (c) => { stdout += c.toString(); });
    child.stderr.on("data", (c) => { stderr += c.toString(); });
    child.on("error", reject);
    child.on("exit", async (code) => {
      if (stderrLog) {
        try { await fs.appendFile(stderrLog, `=== omc team start (exit=${code}) ===\nstdout:\n${stdout}\nstderr:\n${stderr}\n`); }
        catch { /* logging is best-effort */ }
      }
      if (code !== 0) {
        // Detect the one_team_per_leader_session conflict — omc errors with
        // `Leader session already owns active team "<NAME>"`. We surface the
        // stale name to the caller so it can shut it down and retry.
        const m = (stderr + stdout).match(/already owns active team\s+["“]([^"”]+)["”]/);
        const err = new Error(`omc team exited with code ${code}\nstderr tail:\n${stderr.slice(-2000)}`);
        if (m) err.cause = { kind: "stale_leader", staleTeam: m[1] };
        return reject(err);
      }
      const teamName = await resolveTeamNameFromStateDir({ cwd, prompt, spawnStartMs });
      if (!teamName) {
        return reject(new Error(
          `omc team exited 0 but no fresh subdir appeared under ${cwd}/.omc/state/team/.\n` +
          `stdout (first 500): ${stdout.slice(0, 500)}`
        ));
      }
      resolve({ teamName, raw: { stdout, source: "state_dir" } });
    });
  });
}

// Spawn `omc team` and capture the team name. If a previous orchestrator run
// crashed mid-iteration without releasing its leader lock, omc rejects new
// spawns with one_team_per_leader_session. Detect that exact case, shut down
// the stale team, and retry once.
async function startTeam(opts) {
  try {
    return await spawnTeamOnce(opts);
  } catch (err) {
    const cause = err?.cause;
    if (cause?.kind === "stale_leader" && cause.staleTeam) {
      if (opts.onProgress) opts.onProgress({ kind: "stale_team_cleanup", staleTeam: cause.staleTeam });
      await shutdownTeam(cause.staleTeam, opts.cwd);
      return await spawnTeamOnce(opts);
    }
    throw err;
  }
}

function isTerminal(taskCounts) {
  // taskCounts shape: { total, pending, blocked?, in_progress, completed, failed }
  // We treat "all settled" as `pending + blocked + in_progress === 0` AND total > 0.
  if (!taskCounts || typeof taskCounts.total !== "number" || taskCounts.total === 0) {
    return false;
  }
  const inflight =
    (taskCounts.pending ?? 0) +
    (taskCounts.blocked ?? 0) +
    (taskCounts.in_progress ?? 0);
  return inflight === 0;
}

function summaryToCounts(response) {
  // Real shape from `omc team api get-summary --json` (omc 4.13.6):
  //   { ok, data: { summary: { tasks: { total, pending, blocked,
  //     in_progress, completed, failed }, workers: [...], ... } } }
  // Older revisions exposed a flatter shape; we accept both.
  const data = response?.data ?? response ?? {};
  const tasks = data.summary?.tasks ?? data.tasks ?? {};
  const pick = (obj, ...keys) => {
    for (const k of keys) {
      if (obj?.[k] !== undefined && obj[k] !== null) return obj[k];
    }
    return 0;
  };
  return {
    total: pick(tasks, "total", "total_count", "totalCount"),
    pending: pick(tasks, "pending", "pending_count", "pendingCount"),
    blocked: pick(tasks, "blocked", "blocked_count", "blockedCount"),
    in_progress: pick(tasks, "in_progress", "inProgress", "in_progress_count", "inProgressCount"),
    completed: pick(tasks, "completed", "completed_count", "completedCount"),
    failed: pick(tasks, "failed", "failed_count", "failedCount"),
  };
}

// How long to wait for omc team to register at least one task before treating
// the team as stuck. Empty-prompt or atomic-goal cases sometimes sit at total=0
// indefinitely; better to abort early and surface diagnostics than hang.
const ZERO_TASKS_TIMEOUT_MS = 90_000;

async function pollUntilDone({ teamName, cwd, pollIntervalMs, maxWaitMs, abortSignal, onProgress }) {
  const start = Date.now();
  let firstNonZeroTotalAt = null;
  let lastCounts = null;
  while (true) {
    if (abortSignal?.aborted) throw new Error("omc team poll aborted");
    if (Date.now() - start > maxWaitMs) {
      throw new Error(`omc team poll exceeded ${maxWaitMs}ms (last counts: ${JSON.stringify(lastCounts)})`);
    }
    let summary;
    try {
      summary = await omcApi("get-summary", { team_name: teamName }, { cwd });
    } catch (err) {
      // Treat individual API hiccups as transient and retry on next tick.
      onProgress?.({ kind: "api_error", message: err.message });
      await new Promise((r) => setTimeout(r, pollIntervalMs));
      continue;
    }
    const counts = summaryToCounts(summary);
    lastCounts = counts;
    onProgress?.({ kind: "counts", counts });
    if ((counts.total ?? 0) > 0 && firstNonZeroTotalAt === null) {
      firstNonZeroTotalAt = Date.now();
    }
    if (isTerminal(counts)) {
      return { counts, summary };
    }
    if (firstNonZeroTotalAt === null && Date.now() - start > ZERO_TASKS_TIMEOUT_MS) {
      const stateDir = `${cwd}/.omc/state/team/${teamName}`;
      throw new Error(
        `omc team "${teamName}" registered no tasks within ${Math.round(ZERO_TASKS_TIMEOUT_MS / 1000)}s ` +
        `(total=0 throughout). The decomposer may have stalled or classified the goal as atomic ` +
        `with no subtasks. Inspect state for clues:\n` +
        `  ${stateDir}/manifest.json   — team config + governance\n` +
        `  ${stateDir}/monitor-snapshot.json — worker phase + last activity\n` +
        `  ${stateDir}/events.jsonl    — recent events\n` +
        `  ${stateDir}/workers/*/inbox.md — what the workers actually saw`
      );
    }
    await new Promise((r) => setTimeout(r, pollIntervalMs));
  }
}

async function shutdownTeam(teamName, cwd) {
  // Idempotent — if the team is already gone, swallow.
  try {
    await exec("omc", ["team", "shutdown", teamName], { cwd, timeout: 15_000 });
  } catch { /* already gone or never started */ }
}

function renderArtifact({ teamName, counts, tasks, prompt }) {
  const lines = [];
  lines.push(`# omc team execution result`);
  lines.push("");
  lines.push(`Team: ${teamName}`);
  lines.push(`Prompt: ${prompt}`);
  lines.push(`Tasks: total=${counts.total} completed=${counts.completed} failed=${counts.failed}`);
  lines.push("");
  if (Array.isArray(tasks) && tasks.length > 0) {
    lines.push("## Tasks");
    for (const t of tasks) {
      const id = t.id ?? t.task_id ?? "?";
      const status = t.status ?? "?";
      const subject = t.subject ?? t.title ?? "(no subject)";
      const owner = t.owner ?? t.assignee ?? "(unassigned)";
      lines.push(`- [${status}] #${id} (${owner}): ${subject}`);
      const result = t.result ?? t.output ?? t.summary;
      if (result) {
        const trimmed = String(result).slice(0, 1000);
        lines.push("  ```");
        for (const line of trimmed.split("\n")) lines.push(`  ${line}`);
        lines.push("  ```");
      }
    }
  } else {
    lines.push("(no per-task detail available)");
  }
  return lines.join("\n");
}

// Public entry point.
//
// Returns:
//   {
//     ok: bool,                 // true iff failed === 0 AND completed > 0
//     teamName: string,
//     counts: {total, ..., completed, failed},
//     tasks: array,             // raw `list-tasks` response data (best effort)
//     artifact: string,         // markdown summary for ouroboros_evaluate
//     raw: { startResult, summary, listResult },
//   }
export async function runOmcTeam({
  prompt,
  workers = DEFAULT_WORKERS,
  agentType = DEFAULT_AGENT_TYPE,
  cwd = process.cwd(),
  pollIntervalMs = DEFAULT_POLL_INTERVAL_MS,
  maxWaitMs = DEFAULT_MAX_WAIT_MS,
  stderrLog = null,
  abortSignal = null,
  onProgress = null,
}) {
  if (!prompt || !prompt.trim()) throw new Error("runOmcTeam: prompt is required");

  const startResult = await startTeam({ prompt, workers, agentType, cwd, stderrLog, onProgress });
  const teamName = startResult.teamName;
  onProgress?.({ kind: "started", teamName });

  let counts, summary, listResult;
  try {
    ({ counts, summary } = await pollUntilDone({
      teamName, cwd, pollIntervalMs, maxWaitMs, abortSignal, onProgress,
    }));
    listResult = await omcApi("list-tasks", { team_name: teamName }, { cwd });
  } finally {
    await shutdownTeam(teamName, cwd);
  }

  const tasks = listResult?.data?.tasks ?? listResult?.data ?? [];
  const artifact = renderArtifact({ teamName, counts, tasks, prompt });

  return {
    ok: counts.failed === 0 && counts.completed > 0,
    teamName,
    counts,
    tasks,
    artifact,
    raw: { startResult, summary, listResult },
  };
}

// Re-export for tests / introspection.
export const _internal = { slugifyTaskName, summaryToCounts, isTerminal, renderArtifact };

void homedir; // imported eagerly in case future cache logic needs it; keep linter quiet.
