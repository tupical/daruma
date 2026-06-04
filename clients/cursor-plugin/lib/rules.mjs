// Copies the bundled taskagent Cursor Rules (.mdc) into a project's
// `.cursor/rules/` directory. Mirrors what a Cursor marketplace plugin would
// drop alongside the MCP registration.
//
// We ship two rules:
//   - taskagent-policy.mdc — alwaysApply: true; workspace policy that makes
//     taskagent the default tracker and forbids `.omc/plans/` shadow plans.
//   - taskagent.mdc        — alwaysApply: false; the full tool contract,
//     loaded on demand via its description.

import { promises as fs } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SOURCE_DIR = join(__dirname, "..", "cursor", "rules");

export const RULE_FILES = ["taskagent-policy.mdc", "taskagent.mdc"];

export async function installRules({ projectDir, overwrite = false } = {}) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
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
