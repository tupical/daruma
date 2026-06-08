// Copies the bundled taskagent Cursor Rules (.mdc) into a project's
// `.cursor/rules/` directory. Mirrors what a Cursor marketplace plugin would
// drop alongside the MCP registration.
//
// We ship three rules:
//   - taskagent-policy.mdc — alwaysApply: true; workspace policy that makes
//     taskagent the default tracker, forbids `.omc/plans/` shadow plans, and
//     keeps the agent on the token-lean `list active` path.
//   - taskagent.mdc        — alwaysApply: false; the full tool contract plus
//     the audit/close workflow, loaded on demand via its description.
//   - workspacegraph.mdc   — alwaysApply: false; guardrails for the
//     `taskagent_workspacegraph_*` tools so graph search is never used to
//     list open tasks. Without it the graph MCP tools have no guidance and
//     the agent burns tokens on multi-KB graph dumps (see tokensaveaudit.md).

import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { resolvePath } from "./paths.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SOURCE_DIR = join(__dirname, "..", "cursor", "rules");

export const RULE_FILES = [
  "taskagent-policy.mdc",
  "taskagent.mdc",
  "workspacegraph.mdc",
];

export async function installRules({ projectDir, overwrite = false } = {}) {
  const dir = projectDir ? resolvePath(projectDir) : process.cwd();
  const targetDir = join(dir, ".cursor", "rules");
  await fs.mkdir(targetDir, { recursive: true });

  const results = [];
  for (const name of RULE_FILES) {
    const src = join(SOURCE_DIR, name);
    const dst = join(targetDir, name);
    const exists = await fs.access(dst).then(() => true).catch(() => false);
    if (exists && !overwrite) {
      results.push({ path: dst, name, action: "skipped-exists" });
      continue;
    }
    const content = await fs.readFile(src, "utf8");
    await fs.writeFile(dst, content);
    results.push({ path: dst, name, action: exists ? "overwritten" : "installed" });
  }
  return results;
}
