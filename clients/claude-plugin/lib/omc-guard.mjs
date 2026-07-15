// OMC guard — when a project already has a `.omc/` directory
// (oh-my-claudecode artifacts), this drops a managed block into
// `.omc/AGENTS.md` telling OMC-aware agents to route plan persistence
// through daruma instead of writing `.omc/plans/` or
// `.omc/ultragoal/` files. Idempotent: the block is delimited and
// replaced on subsequent runs.
//
// We do NOT touch the OMC plugin itself — this only writes a project-
// local instruction file that OMC agents read alongside other AGENTS.md
// docs. If `.omc/` does not exist we no-op (action: "skipped-no-omc"),
// since there is no OMC state to override.

import { promises as fs } from "node:fs";
import { join, resolve } from "node:path";

const BEGIN = "<!-- daruma-claude:begin -->";
const END = "<!-- daruma-claude:end -->";

const BLOCK_BODY = `# daruma override — task & plan persistence

This project uses the **daruma** MCP server as the single source of
truth for tasks and plans. The daruma-claude Cursor plugin manages
this block; do not hand-edit between the markers.

**For any OMC skill that would normally write under \`.omc/\` plans**
(\`/plan\`, \`/ultragoal\`, \`/autopilot\`, \`/ralph\`, \`/ultrawork\`,
\`/ralplan\`, \`/team\`):

- **Do not create or update** \`.omc/plans/\`, \`.omc/ultragoal/\`, or
  \`.omc/state/plans*\`. Those paths are reserved as no-write for plan
  authoring in this workspace.
- **Author plans via daruma first:** \`daruma_workspace_info\` →
  \`daruma_plan_materialize\` (the plan with its tasks, one atomic call). OMC may still execute, but the plan it
  consumes is fetched via \`daruma_plan_get\` /
  \`daruma_plan_next_task\`.
- **OMC logs, state/sessions, notepad, and research artifacts**
  (\`.omc/logs/\`, \`.omc/state/sessions/\`, \`.omc/notepad.md\`,
  \`.omc/research/\`) remain untouched by this rule — only plan
  persistence is redirected.

If \`daruma_healthz\` fails, surface that to the user and ask them to
start the daruma server. Do not silently fall back to \`.omc/plans/\`.
`;

function buildBlock() {
  return `${BEGIN}\n${BLOCK_BODY}${END}\n`;
}

export async function omcDirExists(projectDir) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
  try {
    const stat = await fs.stat(join(dir, ".omc"));
    return stat.isDirectory();
  } catch {
    return false;
  }
}

// Writes (or refreshes) the managed block in `<projectDir>/.omc/AGENTS.md`.
// Returns one of:
//   { action: "skipped-no-omc", path }       — no .omc/ in this project
//   { action: "installed",     path }        — file created
//   { action: "updated",       path }        — managed block replaced
//   { action: "appended",      path }        — file existed without our block
//   { action: "unchanged",     path }        — content already current
export async function installOmcGuard({ projectDir } = {}) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
  const omcDir = join(dir, ".omc");
  const target = join(omcDir, "AGENTS.md");

  if (!(await omcDirExists(dir))) {
    return { action: "skipped-no-omc", path: target };
  }

  const block = buildBlock();
  let existing = null;
  try {
    existing = await fs.readFile(target, "utf8");
  } catch (err) {
    if (err.code !== "ENOENT") throw err;
  }

  if (existing === null) {
    await fs.writeFile(target, block);
    return { action: "installed", path: target };
  }

  const beginIdx = existing.indexOf(BEGIN);
  const endIdx = existing.indexOf(END);
  if (beginIdx === -1 || endIdx === -1 || endIdx < beginIdx) {
    const sep = existing.endsWith("\n") ? "" : "\n";
    await fs.writeFile(target, `${existing}${sep}\n${block}`);
    return { action: "appended", path: target };
  }

  const before = existing.slice(0, beginIdx);
  const afterStart = endIdx + END.length;
  const after = existing.slice(afterStart).replace(/^\n/, "");
  const next = `${before}${block}${after}`;
  if (next === existing) {
    return { action: "unchanged", path: target };
  }
  await fs.writeFile(target, next);
  return { action: "updated", path: target };
}

// Removes the managed block (and the file if it would be left empty).
export async function removeOmcGuard({ projectDir } = {}) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
  const target = join(dir, ".omc", "AGENTS.md");

  let existing = null;
  try {
    existing = await fs.readFile(target, "utf8");
  } catch (err) {
    if (err.code === "ENOENT") return { action: "missing", path: target };
    throw err;
  }
  const beginIdx = existing.indexOf(BEGIN);
  const endIdx = existing.indexOf(END);
  if (beginIdx === -1 || endIdx === -1) {
    return { action: "missing", path: target };
  }
  const before = existing.slice(0, beginIdx).replace(/\n+$/, "");
  const after = existing.slice(endIdx + END.length).replace(/^\n+/, "");
  const next = [before, after].filter(Boolean).join("\n\n");
  if (next.trim().length === 0) {
    await fs.unlink(target);
    return { action: "removed-file", path: target };
  }
  await fs.writeFile(target, next.endsWith("\n") ? next : `${next}\n`);
  return { action: "removed-block", path: target };
}
